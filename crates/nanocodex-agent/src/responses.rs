use std::num::NonZeroU32;

use nanocodex_oai_api::ResponsesRetryPolicy;
use tower::{
    ServiceBuilder,
    layer::util::{Identity, Stack},
};

use nanocodex_oai_api::{ModelConfig, ResponsesHistory, ResponsesTransport};

/// Marker used until the standard Responses service is constructed by the
/// agent builder.
#[derive(Clone, Copy, Debug, Default)]
pub struct StandardResponses;

/// Deferred Tower layers applied to the standard Responses service when the
/// agent is built.
#[doc(hidden)]
#[derive(Clone)]
pub struct LayeredResponses<L>(pub(crate) ServiceBuilder<L>);

/// Deferred caller service factory used to create one independent stack per
/// conversation branch.
#[doc(hidden)]
#[derive(Clone)]
pub struct FactoryResponses<F>(pub(crate) F);

/// Concrete service factory imported from a configured [`nanocodex_oai_api::OpenAi`].
#[doc(hidden)]
#[derive(Clone)]
pub struct OpenAiResponses<F>(pub(crate) F);

/// Responses transport configuration with standard or caller-supplied Tower
/// service factory policy.
#[derive(Clone)]
pub struct Responses<S = StandardResponses> {
    pub(crate) websocket_url: Option<String>,
    pub(crate) api_base_url: Option<String>,
    #[cfg(not(target_family = "wasm"))]
    pub(crate) http_client: Option<reqwest::Client>,
    pub(crate) transport: ResponsesTransport,
    pub(crate) history: Option<ResponsesHistory>,
    pub(crate) store: Option<bool>,
    pub(crate) max_attempts: NonZeroU32,
    pub(crate) service: S,
}

impl<S> Responses<S> {
    pub(crate) fn from_openai(config: &ModelConfig, service: S) -> Self {
        Self {
            websocket_url: Some(config.websocket_url.clone()),
            api_base_url: Some(config.api_base_url.clone()),
            #[cfg(not(target_family = "wasm"))]
            http_client: None,
            transport: config.responses_transport,
            history: Some(config.responses_history),
            store: Some(config.store_responses),
            max_attempts: ResponsesRetryPolicy::DEFAULT_MAX_ATTEMPTS,
            service,
        }
    }
}

impl Default for Responses<StandardResponses> {
    fn default() -> Self {
        Self {
            websocket_url: None,
            api_base_url: None,
            #[cfg(not(target_family = "wasm"))]
            http_client: None,
            transport: ResponsesTransport::WebSocket,
            history: None,
            store: None,
            max_attempts: ResponsesRetryPolicy::DEFAULT_MAX_ATTEMPTS,
            service: StandardResponses,
        }
    }
}

impl Responses<StandardResponses> {
    /// Starts configuring the legacy agent-local Responses stack.
    ///
    /// New code should prefer [`nanocodex_oai_api::OpenAi::builder`] and pass
    /// the resulting client recipe to [`crate::Nanocodex::builder`].
    #[must_use]
    pub fn builder() -> ResponsesBuilder<StandardResponses> {
        ResponsesBuilder {
            responses: Self::default(),
        }
    }
}

impl ResponsesBuilder<StandardResponses> {
    /// Adds a Tower layer around the SDK's standard Responses transport and
    /// retry service. Layers are not materialized until
    /// [`crate::NanocodexBuilder::build`].
    #[must_use]
    pub fn layer<L>(self, layer: L) -> ResponsesBuilder<LayeredResponses<Stack<L, Identity>>> {
        ResponsesBuilder {
            responses: Responses {
                websocket_url: self.responses.websocket_url,
                api_base_url: self.responses.api_base_url,
                #[cfg(not(target_family = "wasm"))]
                http_client: self.responses.http_client,
                transport: self.responses.transport,
                history: self.responses.history,
                store: self.responses.store,
                max_attempts: self.responses.max_attempts,
                service: LayeredResponses(ServiceBuilder::new().layer(layer)),
            },
        }
    }

    /// Replaces the standard stack with a factory that constructs one fresh
    /// caller-composed service for the root and every child or fork.
    #[must_use]
    pub fn service<F, S>(self, factory: F) -> ResponsesBuilder<FactoryResponses<F>>
    where
        F: Fn() -> S,
    {
        ResponsesBuilder {
            responses: Responses {
                websocket_url: self.responses.websocket_url,
                api_base_url: self.responses.api_base_url,
                #[cfg(not(target_family = "wasm"))]
                http_client: self.responses.http_client,
                transport: self.responses.transport,
                history: self.responses.history,
                store: self.responses.store,
                max_attempts: self.responses.max_attempts,
                service: FactoryResponses(factory),
            },
        }
    }
}

impl<L> ResponsesBuilder<LayeredResponses<L>> {
    /// Adds another Tower layer to the deferred standard service stack.
    #[must_use]
    pub fn layer<T>(self, layer: T) -> ResponsesBuilder<LayeredResponses<Stack<T, L>>> {
        ResponsesBuilder {
            responses: Responses {
                websocket_url: self.responses.websocket_url,
                api_base_url: self.responses.api_base_url,
                #[cfg(not(target_family = "wasm"))]
                http_client: self.responses.http_client,
                transport: self.responses.transport,
                history: self.responses.history,
                store: self.responses.store,
                max_attempts: self.responses.max_attempts,
                service: LayeredResponses(self.responses.service.0.layer(layer)),
            },
        }
    }
}

/// Builder for the standard Responses endpoints or a caller-composed service.
pub struct ResponsesBuilder<S> {
    responses: Responses<S>,
}

impl<S> ResponsesBuilder<S> {
    /// Selects the transport once for the complete lifetime of an agent and
    /// every child or fork created from it.
    #[must_use]
    pub const fn transport(mut self, transport: ResponsesTransport) -> Self {
        self.responses.transport = transport;
        self
    }

    /// Selects incremental response-ID chaining or complete history replay.
    ///
    /// When omitted, HTTPS with `store: false` selects full replay and all
    /// other combinations select incremental chaining.
    #[must_use]
    pub const fn history(mut self, history: ResponsesHistory) -> Self {
        self.responses.history = Some(history);
        self
    }

    /// Controls whether Responses checkpoints are retained by the API.
    ///
    /// The default is `true` for API-key authentication and `false` for
    /// `ChatGPT` subscription authentication.
    #[must_use]
    pub const fn store(mut self, store: bool) -> Self {
        self.responses.store = Some(store);
        self
    }

    /// Sets the maximum number of total attempts made by the SDK's standard
    /// Responses retry stack, including the initial request.
    ///
    /// Set this to one when replaying a request could repeat an external side
    /// effect, such as an up-front payment. Caller-supplied service factories
    /// own their own retry policy.
    #[must_use]
    pub const fn max_attempts(mut self, max_attempts: NonZeroU32) -> Self {
        self.responses.max_attempts = max_attempts;
        self
    }

    /// Replaces the persistent Responses WebSocket endpoint.
    #[must_use]
    pub fn websocket_url(mut self, url: impl Into<String>) -> Self {
        self.responses.websocket_url = Some(url.into());
        self
    }

    /// Replaces the HTTPS Responses API base URL.
    #[must_use]
    pub fn api_base_url(mut self, url: impl Into<String>) -> Self {
        self.responses.api_base_url = Some(url.into());
        self
    }

    /// Replaces the client used by the standard HTTPS Responses transport.
    ///
    /// This permits caller-owned proxy, certificate, and connection-pool
    /// policy without changing the Responses service or retry boundary.
    #[cfg(not(target_family = "wasm"))]
    #[must_use]
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.responses.http_client = Some(client);
        self
    }

    /// Finishes this deferred Responses configuration.
    #[must_use]
    pub fn build(self) -> Responses<S> {
        self.responses
    }
}
