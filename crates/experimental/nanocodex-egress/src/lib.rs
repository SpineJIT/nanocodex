//! Composable authenticated HTTP egress proxy.
//!
//! [`EgressProxy`] owns the loopback proxy, TLS interception, bounded request
//! forwarding, and lifecycle. Applications compose protocol behavior through
//! ordered [`EgressLayer`] implementations. Nanocodex uses that seam for MPP
//! payment and replay while keeping wallet material in the host process.

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

/// The intentional extension seam for an application-defined [`EgressLayer`].
///
/// Reexports keep middleware versioning owned by this crate while allowing an
/// application to adapt an existing middleware, as the Nanocodex binary does
/// for Tempo MPP.
pub mod middleware {
    pub use async_trait::async_trait;
    pub use http::Extensions;
    pub use reqwest::{Request, Response};
    pub use reqwest_middleware::{Error, Middleware, Next, Result};
}

use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::TryStreamExt;
use http_body_util::{BodyExt, Limited};
use hudsucker::{
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
    certificate_authority::CertificateAuthority,
    hyper::{
        Method, Request, Response, StatusCode,
        header::{
            CONNECTION, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TRANSFER_ENCODING, UPGRADE,
        },
        http::uri::Authority,
    },
    rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
        IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, string::Ia5String,
    },
    rustls::{
        ServerConfig,
        crypto::ring,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, Middleware, Next};
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot},
    task::JoinHandle,
};
use tracing::Instrument as _;

const DEFAULT_MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_CONCURRENT_CONNECTIONS: usize = 128;
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 128;
const MAX_IDLE_CONNECTIONS_PER_ORIGIN: usize = 4;
const CA_FILENAME: &str = "mpp-egress-ca.pem";

/// One independently composable outbound HTTP behavior.
///
/// Layers receive ordinary forwarded requests in attachment order.
#[async_trait]
pub trait EgressLayer: Send + Sync + 'static {
    /// Handles one replayable outbound request and optionally invokes the rest
    /// of the stack with [`Next::run`].
    async fn handle(
        &self,
        request: reqwest::Request,
        extensions: &mut ::http::Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response>;
}

/// Private transport policy owned by one embedded proxy instance.
#[derive(Clone, Debug)]
struct ProxyPolicy {
    max_request_bytes: usize,
    max_concurrent_requests: usize,
    max_concurrent_connections: usize,
}

impl Default for ProxyPolicy {
    fn default() -> Self {
        Self {
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            max_concurrent_connections: DEFAULT_MAX_CONCURRENT_CONNECTIONS,
        }
    }
}

struct LayerMiddleware(Arc<dyn EgressLayer>);

#[async_trait]
impl Middleware for LayerMiddleware {
    async fn handle(
        &self,
        request: reqwest::Request,
        extensions: &mut ::http::Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        self.0.handle(request, extensions, next).await
    }
}

/// A running authenticated loopback proxy and its ephemeral certificate authority.
pub struct EgressProxy {
    proxy_url: String,
    proxy_password: String,
    proxy_authorization: String,
    ca_certificate_path: PathBuf,
    _temp_dir: TempDir,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), hudsucker::Error>>>,
    #[cfg(test)]
    _test_permit: tokio::sync::OwnedSemaphorePermit,
}

impl EgressProxy {
    /// Starts a composable proxy builder with no outbound layers.
    #[must_use]
    pub fn builder() -> EgressProxyBuilder {
        EgressProxyBuilder::new()
    }

    async fn start(
        policy: ProxyPolicy,
        layers: Vec<Arc<dyn EgressLayer>>,
    ) -> Result<Self, EgressError> {
        #[cfg(test)]
        let test_permit = test_proxy_permit().await;
        if policy.max_request_bytes == 0 {
            return Err(EgressError::ZeroMaxRequestBytes);
        }
        if policy.max_concurrent_requests == 0 {
            return Err(EgressError::ZeroMaxConcurrentRequests);
        }
        let max_concurrent_connections = NonZeroUsize::new(policy.max_concurrent_connections)
            .ok_or(EgressError::ZeroMaxConcurrentConnections)?;

        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .map_err(EgressError::Bind)?;
        let address = listener.local_addr().map_err(EgressError::LocalAddress)?;
        let (authority, certificate_pem) = ephemeral_authority()?;
        let temp_dir = tempfile::Builder::new()
            .prefix("nanocodex-mpp-egress-")
            .tempdir()
            .map_err(EgressError::TempDir)?;
        let ca_certificate_path = temp_dir.path().join(CA_FILENAME);
        std::fs::write(&ca_certificate_path, &certificate_pem)
            .map_err(EgressError::WriteCertificate)?;

        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(MAX_IDLE_CONNECTIONS_PER_ORIGIN)
            .build()
            .map_err(EgressError::Client)?;
        let mut client_builder = ClientBuilder::new(client);
        for layer in &layers {
            client_builder = client_builder.with(LayerMiddleware(Arc::clone(layer)));
        }
        let client = client_builder.build();
        let proxy_password = random_proxy_password();
        let proxy_authorization = format!(
            "Basic {}",
            STANDARD.encode(format!("nanocodex:{proxy_password}"))
        );
        let proxy_url = format!("http://nanocodex:{proxy_password}@{address}");
        let origin_permits = Arc::new(Semaphore::new(policy.max_concurrent_requests));
        let handler = ProxyHandler {
            client,
            policy,
            origin_permits,
            authentication: ProxyAuthentication {
                authorization: proxy_authorization.clone().into(),
                tunnel_authenticated: false,
            },
            request_ids: Arc::new(AtomicU64::new(1)),
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(authority)
            .with_rustls_connector(ring::default_provider())
            .with_max_concurrent_connections(max_concurrent_connections)
            .with_http_handler(handler)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .build()?;
        let task = tokio::spawn(proxy.start());

        Ok(Self {
            proxy_url,
            proxy_password,
            proxy_authorization,
            ca_certificate_path,
            _temp_dir: temp_dir,
            shutdown_tx: Some(shutdown_tx),
            task: Some(task),
            #[cfg(test)]
            _test_permit: test_permit,
        })
    }

    /// Returns the authenticated HTTP proxy URL.
    ///
    /// The URL contains a short-lived bearer credential and must not be logged.
    #[must_use]
    pub fn proxy_url(&self) -> String {
        self.proxy_url.clone()
    }

    /// Returns environment overrides for curl and common HTTP runtimes.
    ///
    /// These values contain the proxy bearer capability. They should be applied
    /// only to tool child processes, not logged or installed in the embedding
    /// process, so model/control-plane traffic is not intercepted.
    #[must_use]
    pub fn environment(&self) -> Vec<(OsString, OsString)> {
        let proxy = OsString::from(self.proxy_url());
        let certificate = self.ca_certificate_path.clone().into_os_string();
        [
            ("http_proxy", proxy.clone()),
            ("https_proxy", proxy.clone()),
            ("HTTP_PROXY", proxy.clone()),
            ("HTTPS_PROXY", proxy),
            ("no_proxy", OsString::new()),
            ("NO_PROXY", OsString::new()),
            ("CURL_CA_BUNDLE", certificate.clone()),
            ("SSL_CERT_FILE", certificate.clone()),
            ("REQUESTS_CA_BUNDLE", certificate.clone()),
            ("NODE_EXTRA_CA_CERTS", certificate),
            (
                "NANOCODEX_MPP_EGRESS_PASSWORD",
                OsString::from(&self.proxy_password),
            ),
            (
                "NANOCODEX_MPP_EGRESS_AUTHORIZATION",
                OsString::from(&self.proxy_authorization),
            ),
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), value))
        .collect()
    }

    /// Stops accepting traffic and waits for active proxy connections to drain.
    ///
    /// # Errors
    ///
    /// Returns an error if the proxy task fails or cannot be joined.
    pub async fn shutdown(mut self) -> Result<(), EgressError> {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.map_err(EgressError::Join)??;
        }
        Ok(())
    }

    /// Path to the public ephemeral CA certificate.
    #[must_use]
    pub fn ca_certificate_path(&self) -> &Path {
        &self.ca_certificate_path
    }
}

#[cfg(test)]
async fn test_proxy_permit() -> tokio::sync::OwnedSemaphorePermit {
    static PERMITS: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    Arc::clone(PERMITS.get_or_init(|| Arc::new(Semaphore::new(4))))
        .acquire_owned()
        .await
        .unwrap_or_else(|error| unreachable!("test proxy semaphore closed: {error}"))
}

/// Builder for one bounded proxy and its ordered outbound layers.
pub struct EgressProxyBuilder {
    policy: ProxyPolicy,
    layers: Vec<Arc<dyn EgressLayer>>,
}

impl EgressProxyBuilder {
    /// Creates a builder with bounded defaults and direct HTTP forwarding.
    #[must_use]
    pub fn new() -> Self {
        Self {
            policy: ProxyPolicy::default(),
            layers: Vec::new(),
        }
    }

    /// Sets the maximum replayable request-body size accepted from a child.
    #[must_use]
    pub const fn max_request_bytes(mut self, max_bytes: usize) -> Self {
        self.policy.max_request_bytes = max_bytes;
        self
    }

    /// Sets the maximum number of requests concurrently forwarded to origins.
    ///
    /// Additional child requests wait locally before entering outbound layers.
    #[must_use]
    pub const fn max_concurrent_requests(mut self, max_requests: usize) -> Self {
        self.policy.max_concurrent_requests = max_requests;
        self
    }

    /// Sets the maximum number of accepted child proxy connections.
    ///
    /// Additional clients remain in the listener backlog before consuming a
    /// process file descriptor.
    #[must_use]
    pub const fn max_concurrent_connections(mut self, max_connections: usize) -> Self {
        self.policy.max_concurrent_connections = max_connections;
        self
    }

    /// Appends one independently owned outbound behavior.
    ///
    /// Layers run in attachment order on the initial request. A layer controls
    /// whether and how the remaining stack runs through [`Next::run`].
    #[must_use]
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: EgressLayer,
    {
        self.layers.push(Arc::new(layer));
        self
    }

    /// Starts the proxy and its owned background task.
    ///
    /// # Errors
    ///
    /// Returns a typed initialization or proxy error.
    pub async fn spawn(self) -> Result<EgressProxy, EgressError> {
        EgressProxy::start(self.policy, self.layers).await
    }
}

impl Default for EgressProxyBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone)]
struct ProxyHandler {
    client: ClientWithMiddleware,
    policy: ProxyPolicy,
    origin_permits: Arc<Semaphore>,
    authentication: ProxyAuthentication,
    request_ids: Arc<AtomicU64>,
}

#[derive(Clone)]
struct ProxyAuthentication {
    authorization: Arc<str>,
    // The proxy backend carries the mutated CONNECT handler into only that
    // intercepted stream; clones for unrelated client connections remain false.
    tunnel_authenticated: bool,
}

impl ProxyAuthentication {
    fn authorize(&mut self, request: &Request<Body>) -> bool {
        let has_authorization = request
            .headers()
            .get(PROXY_AUTHORIZATION)
            .is_some_and(|value| value.as_bytes() == self.authorization.as_bytes());
        if has_authorization {
            if request.method() == Method::CONNECT {
                self.tunnel_authenticated = true;
            }
            return true;
        }
        self.tunnel_authenticated
    }
}

impl HttpHandler for ProxyHandler {
    async fn handle_request(
        &mut self,
        context: &HttpContext,
        mut request: Request<Body>,
    ) -> RequestOrResponse {
        let request_id = self.request_ids.fetch_add(1, Ordering::Relaxed);
        let span = tracing::info_span!(
            target: "mpp_egress",
            "mpp.egress.request",
            request.id = request_id,
            mpp.request.id = tracing::field::Empty,
            client.address = %context.client_addr,
            http.request.method = %request.method(),
            url.full = %request.uri(),
            request.upgrade = is_upgrade(&request),
        );
        async move {
            tracing::info!(
                target: "mpp_egress",
                content_kind = "mpp.egress.request.headers",
                content = ?request.headers(),
                "trace content"
            );
            if !self.authentication.authorize(&request) {
                tracing::warn!(
                    target: "mpp_egress",
                    stage = "mpp.egress.proxy_authentication.rejected",
                    http.response.status_code = StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16(),
                    "MPP egress rejected an unauthenticated client"
                );
                return proxy_authentication_required().into();
            }
            tracing::info!(
                target: "mpp_egress",
                stage = "mpp.egress.proxy_authentication.accepted",
                "MPP egress authenticated its child client"
            );
            request.headers_mut().remove(PROXY_AUTHORIZATION);
            if request.method() == Method::CONNECT || is_upgrade(&request) {
                tracing::info!(
                    target: "mpp_egress",
                    stage = "mpp.egress.tunnel.forwarded",
                    "MPP egress forwarded a protocol tunnel without payment handling"
                );
                return request.into();
            }

            match self.forward(request).await {
                Ok(response) => response.into(),
                Err(ForwardError::RequestTooLarge) => {
                    tracing::warn!(
                        target: "mpp_egress",
                        stage = "mpp.egress.request.failed",
                        failure.kind = "request_too_large",
                        http.response.status_code = StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
                        "MPP egress rejected an unreplayable request body"
                    );
                    error_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request body exceeds the MPP egress replay limit",
                    )
                    .into()
                }
                Err(error) => {
                    tracing::warn!(
                        target: "mpp_egress",
                        stage = "mpp.egress.request.failed",
                        failure.kind = "payment_or_forwarding",
                        http.response.status_code = StatusCode::BAD_GATEWAY.as_u16(),
                        error = %error,
                        "MPP egress request failed"
                    );
                    error_response(StatusCode::BAD_GATEWAY, &error.to_string()).into()
                }
            }
        }
        .instrument(span)
        .await
    }
}

impl ProxyHandler {
    async fn forward(&self, request: Request<Body>) -> Result<Response<Body>, ForwardError> {
        let (mut parts, body) = request.into_parts();
        let body = Limited::new(body, self.policy.max_request_bytes)
            .collect()
            .await
            .map_err(|_| ForwardError::RequestTooLarge)?
            .to_bytes();
        record_body_content("mpp.egress.request.body", &body);
        remove_hop_by_hop_request_headers(&mut parts.headers);
        let queued = self.origin_permits.available_permits() == 0;
        if queued {
            tracing::info!(
                target: "mpp_egress",
                stage = "mpp.egress.origin.request.queued",
                origin.max_concurrent_requests = self.policy.max_concurrent_requests,
                "MPP egress queued the request before contacting its origin"
            );
        }
        let _origin_permit = self
            .origin_permits
            .acquire()
            .await
            .map_err(|_| ForwardError::Unavailable)?;
        tracing::info!(
            target: "mpp_egress",
            stage = "mpp.egress.origin.request.started",
            http.request.body.size = body.len(),
            request.queued = queued,
            "MPP egress sent the original request"
        );

        let builder = self
            .client
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body);
        let response = builder.send().await.map_err(ForwardError::Layer)?;

        let status = response.status();
        tracing::info!(
            target: "mpp_egress",
            stage = "mpp.egress.request.completed",
            http.response.status_code = status.as_u16(),
            "MPP egress completed the request"
        );
        Ok(convert_response(response, &tracing::Span::current()))
    }
}

fn record_body_content(kind: &'static str, body: &[u8]) {
    if !tracing::enabled!(target: "mpp_egress", tracing::Level::INFO) {
        return;
    }
    if let Ok(content) = std::str::from_utf8(body) {
        tracing::info!(
            target: "mpp_egress",
            content_kind = kind,
            content,
            "trace content"
        );
    } else {
        tracing::info!(
            target: "mpp_egress",
            content_kind = kind,
            content = ?body,
            "trace content"
        );
    }
}

fn convert_response(mut response: reqwest::Response, span: &tracing::Span) -> Response<Body> {
    let status = response.status();
    let version = response.version();
    let mut headers = std::mem::take(response.headers_mut());
    remove_hop_by_hop_response_headers(&mut headers);
    let trace_content = tracing::enabled!(target: "mpp_egress", tracing::Level::INFO);
    if trace_content {
        span.in_scope(|| {
            tracing::info!(
                target: "mpp_egress",
                content_kind = "mpp.egress.response.headers",
                content = ?headers,
                "trace content"
            );
        });
    }
    let stream = response.bytes_stream();
    let body = if trace_content {
        let content_span = span.clone();
        let mut chunk_index = 0_u64;
        Body::from_stream(
            stream
                .map_ok(move |chunk| {
                    content_span.in_scope(|| {
                        if let Ok(content) = std::str::from_utf8(&chunk) {
                            tracing::info!(
                                target: "mpp_egress",
                                content_kind = "mpp.egress.response.body",
                                response.chunk.index = chunk_index,
                                response.chunk.size = chunk.len(),
                                content,
                                "trace content"
                            );
                        } else {
                            tracing::info!(
                                target: "mpp_egress",
                                content_kind = "mpp.egress.response.body",
                                response.chunk.index = chunk_index,
                                response.chunk.size = chunk.len(),
                                content = ?chunk.as_ref(),
                                "trace content"
                            );
                        }
                    });
                    chunk_index = chunk_index.saturating_add(1);
                    chunk
                })
                .map_err(|_| hudsucker::Error::Unknown),
        )
    } else {
        Body::from_stream(stream.map_err(|_| hudsucker::Error::Unknown))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.version_mut() = version;
    *response.headers_mut() = headers;
    response
}

fn is_upgrade(request: &Request<Body>) -> bool {
    request.headers().contains_key(UPGRADE)
        || request
            .headers()
            .get(CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
}

fn remove_hop_by_hop_request_headers(headers: &mut hudsucker::hyper::HeaderMap) {
    remove_connection_named_headers(headers);
    for name in [
        CONNECTION,
        HOST,
        PROXY_AUTHORIZATION,
        TRANSFER_ENCODING,
        UPGRADE,
    ] {
        headers.remove(name);
    }
}

fn remove_hop_by_hop_response_headers(headers: &mut hudsucker::hyper::HeaderMap) {
    remove_connection_named_headers(headers);
    for name in [CONNECTION, TRANSFER_ENCODING, UPGRADE] {
        headers.remove(name);
    }
}

fn remove_connection_named_headers(headers: &mut hudsucker::hyper::HeaderMap) {
    let names = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| {
            name.trim()
                .parse::<hudsucker::hyper::header::HeaderName>()
                .ok()
        })
        .collect::<Vec<_>>();
    for name in names {
        headers.remove(name);
    }
}

fn error_response(status: StatusCode, message: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(message.to_owned()));
    *response.status_mut() = status;
    response
}

fn proxy_authentication_required() -> Response<Body> {
    let mut response = error_response(
        StatusCode::PROXY_AUTHENTICATION_REQUIRED,
        "proxy authentication required",
    );
    response.headers_mut().insert(
        PROXY_AUTHENTICATE,
        hudsucker::hyper::header::HeaderValue::from_static("Basic realm=\"nanocodex-mpp-egress\""),
    );
    response
}

fn random_proxy_password() -> String {
    random_identifier()
}

fn random_identifier() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut identifier = String::with_capacity(64);
    for byte in rand::random::<[u8; 32]>() {
        identifier.push(char::from(HEX[usize::from(byte >> 4)]));
        identifier.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    identifier
}

struct EphemeralAuthority {
    issuer: Issuer<'static, KeyPair>,
    private_key: PrivateKeyDer<'static>,
    cache: Mutex<std::collections::HashMap<Authority, Arc<ServerConfig>>>,
}

impl CertificateAuthority for EphemeralAuthority {
    async fn gen_server_config(&self, authority: &Authority) -> Arc<ServerConfig> {
        if let Ok(cache) = self.cache.lock()
            && let Some(config) = cache.get(authority)
        {
            return Arc::clone(config);
        }

        let mut params = CertificateParams::default();
        params.serial_number = Some(rand::random::<u64>().into());
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CommonName, authority.host());
        params.distinguished_name = distinguished_name;
        params
            .subject_alt_names
            .push(authority.host().parse::<IpAddr>().map_or_else(
                |_| {
                    SanType::DnsName(
                        Ia5String::try_from(authority.host())
                            .expect("HTTP authority host must be a valid DNS IA5 string"),
                    )
                },
                SanType::IpAddress,
            ));
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let certificate = params
            .signed_by(self.issuer.key(), &self.issuer)
            .expect("valid CA parameters must sign an ephemeral leaf certificate");
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate)],
                self.private_key.clone_key(),
            )
            .expect("generated leaf certificate and private key must match");
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);

        if let Ok(mut cache) = self.cache.lock()
            && cache.len() < 1_024
        {
            cache.insert(authority.clone(), Arc::clone(&config));
        }
        config
    }
}

fn ephemeral_authority() -> Result<(EphemeralAuthority, String), EgressError> {
    let key_pair = KeyPair::generate().map_err(EgressError::Certificate)?;
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, "Nanocodex ephemeral egress");
    params.distinguished_name = distinguished_name;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let certificate = params
        .self_signed(&key_pair)
        .map_err(EgressError::Certificate)?;
    let certificate_pem = certificate.pem();
    let private_key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    let issuer =
        Issuer::from_ca_cert_pem(&certificate_pem, key_pair).map_err(EgressError::Certificate)?;
    Ok((
        EphemeralAuthority {
            issuer,
            private_key,
            cache: Mutex::new(std::collections::HashMap::new()),
        },
        certificate_pem,
    ))
}

/// Failure to configure, start, run, or stop an egress proxy.
#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    /// The replayable request-body limit was zero.
    #[error("egress max request bytes must be greater than zero")]
    ZeroMaxRequestBytes,
    /// The forwarded-request concurrency limit was zero.
    #[error("egress max concurrent requests must be greater than zero")]
    ZeroMaxConcurrentRequests,
    /// The accepted-connection concurrency limit was zero.
    #[error("egress max concurrent connections must be greater than zero")]
    ZeroMaxConcurrentConnections,
    /// The loopback listener could not be bound.
    #[error("failed to bind the egress proxy listener")]
    Bind(#[source] std::io::Error),
    /// The bound listener address could not be read.
    #[error("failed to read the egress proxy listener address")]
    LocalAddress(#[source] std::io::Error),
    /// The private ephemeral-CA directory could not be created.
    #[error("failed to create the ephemeral egress directory")]
    TempDir(#[source] std::io::Error),
    /// The public CA certificate could not be persisted for child runtimes.
    #[error("failed to write the ephemeral egress CA certificate")]
    WriteCertificate(#[source] std::io::Error),
    /// Ephemeral CA or leaf-certificate generation failed.
    #[error("failed to generate the ephemeral egress CA")]
    Certificate(#[source] hudsucker::rcgen::Error),
    /// The origin-facing HTTP client could not be built.
    #[error("failed to build the egress HTTP client")]
    Client(#[source] reqwest::Error),
    /// The proxy rejected its configuration or failed while serving.
    #[error("egress proxy failed")]
    Proxy(#[from] hudsucker::Error),
    /// The background proxy task panicked or was cancelled unexpectedly.
    #[error("egress proxy task failed")]
    Join(#[source] tokio::task::JoinError),
}

#[derive(Debug, thiserror::Error)]
enum ForwardError {
    #[error("request body is too large to replay")]
    RequestTooLarge,
    #[error("egress proxy stopped while the request was queued")]
    Unavailable,
    #[error("egress layer or origin request failed: {0}")]
    Layer(#[source] reqwest_middleware::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use axum::{
        Router,
        extract::Request,
        http::StatusCode as AxumStatus,
        routing::{get, post},
    };
    use futures_util::future::join_all;

    #[test]
    fn proxy_authentication_only_retains_connect_tunnels() {
        let mut authentication = ProxyAuthentication {
            authorization: Arc::from("Basic test-credential"),
            tunnel_authenticated: false,
        };
        let authorized_request = |method| {
            Request::builder()
                .method(method)
                .header(PROXY_AUTHORIZATION, "Basic test-credential")
                .body(Body::empty())
                .unwrap()
        };
        let unauthenticated_request = || Request::new(Body::empty());

        assert!(authentication.authorize(&authorized_request(Method::GET)));
        assert!(!authentication.tunnel_authenticated);
        assert!(!authentication.authorize(&unauthenticated_request()));

        assert!(authentication.authorize(&authorized_request(Method::CONNECT)));
        assert!(authentication.tunnel_authenticated);
        assert!(authentication.clone().authorize(&unauthenticated_request()));

        let mut fresh_connection = ProxyAuthentication {
            authorization: Arc::clone(&authentication.authorization),
            tunnel_authenticated: false,
        };
        assert!(!fresh_connection.authorize(&unauthenticated_request()));
    }

    struct HeaderLayer(&'static str);

    #[async_trait]
    impl EgressLayer for HeaderLayer {
        async fn handle(
            &self,
            mut request: reqwest::Request,
            extensions: &mut ::http::Extensions,
            next: Next<'_>,
        ) -> reqwest_middleware::Result<reqwest::Response> {
            let previous = request
                .headers()
                .get("x-egress-layers")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            let value = if previous.is_empty() {
                self.0.to_owned()
            } else {
                format!("{previous},{}", self.0)
            };
            request
                .headers_mut()
                .insert("x-egress-layers", value.parse().unwrap());
            next.run(request, extensions).await
        }
    }

    struct DenyLayer;

    #[async_trait]
    impl EgressLayer for DenyLayer {
        async fn handle(
            &self,
            _request: reqwest::Request,
            _extensions: &mut ::http::Extensions,
            _next: Next<'_>,
        ) -> reqwest_middleware::Result<reqwest::Response> {
            let mut response = ::http::Response::new(reqwest::Body::from("denied"));
            *response.status_mut() = reqwest::StatusCode::FORBIDDEN;
            Ok(response.into())
        }
    }

    #[tokio::test]
    async fn builder_reports_each_zero_transport_limit_without_binding() {
        assert!(matches!(
            EgressProxy::builder().max_request_bytes(0).spawn().await,
            Err(EgressError::ZeroMaxRequestBytes)
        ));
        assert!(matches!(
            EgressProxy::builder()
                .max_concurrent_requests(0)
                .spawn()
                .await,
            Err(EgressError::ZeroMaxConcurrentRequests)
        ));
        assert!(matches!(
            EgressProxy::builder()
                .max_concurrent_connections(0)
                .spawn()
                .await,
            Err(EgressError::ZeroMaxConcurrentConnections)
        ));
    }

    async fn spawn_origin(app: Router) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{address}")
    }

    fn proxied_client(egress: &EgressProxy) -> reqwest::Client {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(egress.proxy_url()).unwrap())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn rejects_clients_without_the_ephemeral_proxy_credential() {
        let egress = EgressProxy::builder().spawn().await.unwrap();
        let mut proxy: reqwest::Url = egress.proxy_url().parse().unwrap();
        proxy.set_username("").unwrap();
        proxy.set_password(None).unwrap();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(proxy).unwrap())
            .build()
            .unwrap();

        let response = client.get("http://example.invalid/").send().await.unwrap();

        assert_eq!(response.status(), AxumStatus::PROXY_AUTHENTICATION_REQUIRED);
        egress.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn passes_ordinary_http_responses_through() {
        let origin = spawn_origin(Router::new().route("/plain", get(|| async { "plain" }))).await;
        let egress = EgressProxy::builder().spawn().await.unwrap();

        let response = proxied_client(&egress)
            .get(format!("{origin}/plain"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), AxumStatus::OK);
        assert_eq!(response.text().await.unwrap(), "plain");
        egress.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn layers_run_in_attachment_order() {
        let origin = spawn_origin(Router::new().route(
            "/layers",
            get(|request: Request| async move {
                request
                    .headers()
                    .get("x-egress-layers")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned()
            }),
        ))
        .await;
        let egress = EgressProxy::builder()
            .layer(HeaderLayer("first"))
            .layer(HeaderLayer("second"))
            .spawn()
            .await
            .unwrap();

        let body = proxied_client(&egress)
            .get(format!("{origin}/layers"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert_eq!(body, "first,second");
        egress.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_layer_can_short_circuit_before_the_origin() {
        let calls = Arc::new(AtomicUsize::new(0));
        let route_calls = Arc::clone(&calls);
        let origin = spawn_origin(Router::new().route(
            "/denied",
            get(move || {
                let route_calls = Arc::clone(&route_calls);
                async move {
                    route_calls.fetch_add(1, Ordering::SeqCst);
                    "unexpected"
                }
            }),
        ))
        .await;
        let egress = EgressProxy::builder()
            .layer(DenyLayer)
            .spawn()
            .await
            .unwrap();

        let response = proxied_client(&egress)
            .get(format!("{origin}/denied"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), AxumStatus::FORBIDDEN);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        egress.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queues_excess_requests_before_contacting_the_origin() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(Semaphore::new(0));
        let app = Router::new().route(
            "/bounded",
            get({
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                let started = Arc::clone(&started);
                let gate = Arc::clone(&gate);
                move || {
                    let active = Arc::clone(&active);
                    let maximum = Arc::clone(&maximum);
                    let started = Arc::clone(&started);
                    let gate = Arc::clone(&gate);
                    async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(current, Ordering::SeqCst);
                        started.notify_one();
                        let permit = gate.acquire().await.unwrap();
                        permit.forget();
                        active.fetch_sub(1, Ordering::SeqCst);
                        "bounded"
                    }
                }
            }),
        );
        let origin = spawn_origin(app).await;
        let egress = EgressProxy::builder()
            .max_concurrent_requests(3)
            .spawn()
            .await
            .unwrap();
        let client = proxied_client(&egress);
        let requests = (0..12)
            .map(|_| {
                let client = client.clone();
                let url = format!("{origin}/bounded");
                tokio::spawn(async move { client.get(url).send().await.unwrap().status() })
            })
            .collect::<Vec<_>>();

        while maximum.load(Ordering::SeqCst) < 3 {
            started.notified().await;
        }
        assert_eq!(active.load(Ordering::SeqCst), 3);

        gate.add_permits(12);
        let statuses = join_all(requests)
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert!(statuses.iter().all(|status| *status == AxumStatus::OK));
        assert_eq!(maximum.load(Ordering::SeqCst), 3);
        egress.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_request_bodies_above_the_replay_limit() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_route = Arc::clone(&calls);
        let origin = spawn_origin(Router::new().route(
            "/upload",
            post(move || {
                let calls = Arc::clone(&calls_for_route);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    "unexpected"
                }
            }),
        ))
        .await;
        let egress = EgressProxy::builder()
            .max_request_bytes(4)
            .spawn()
            .await
            .unwrap();

        let response = proxied_client(&egress)
            .post(format!("{origin}/upload"))
            .body("too-large")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), AxumStatus::PAYLOAD_TOO_LARGE);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        egress.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn child_environment_matches_the_existing_mpp_proxy_contract() {
        let egress = EgressProxy::builder().spawn().await.unwrap();
        let environment = egress.environment();
        let value = |name: &str| {
            environment
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.clone())
                .unwrap()
        };

        assert_eq!(value("https_proxy"), OsString::from(egress.proxy_url()));
        assert!(value("NO_PROXY").is_empty());
        assert_eq!(
            PathBuf::from(value("CURL_CA_BUNDLE")),
            egress.ca_certificate_path()
        );
        assert!(egress.ca_certificate_path().is_file());
        assert_eq!(
            value("NANOCODEX_MPP_EGRESS_PASSWORD"),
            OsString::from(&egress.proxy_password)
        );
        assert_eq!(
            value("NANOCODEX_MPP_EGRESS_AUTHORIZATION"),
            OsString::from(&egress.proxy_authorization)
        );
        egress.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "manual public-network HTTPS smoke"]
    async fn live_https_mitm_smoke() {
        let egress = EgressProxy::builder().spawn().await.unwrap();
        let environment = egress.environment();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new("curl")
                .args(["--fail", "--silent", "--show-error", "https://example.com/"])
                .envs(environment)
                .output()
        })
        .await
        .unwrap()
        .unwrap();

        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Example Domain"));
        egress.shutdown().await.unwrap();
    }
}
