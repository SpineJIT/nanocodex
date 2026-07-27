use std::{num::NonZeroU32, sync::Arc};

use tower::Layer;

use crate::{
    DefaultResponsesService, ModelConfig, OpenAiAuth, OpenAiAuthError, OpenAiAuthMode,
    ReasoningMode, ResponsesHistory, ResponsesRetryPolicy, ResponsesService, ResponsesTransport,
    Thinking, session::SessionBuilder,
};

/// Configured, cloneable `OpenAI` client recipe.
///
/// `OpenAi` owns authentication, endpoint policy, and the concrete Tower
/// service factory. Each session built from it receives independent mutable
/// service and conversation state.
#[derive(Clone)]
pub struct OpenAi<F = StandardServiceFactory> {
    config: ModelConfig,
    factory: F,
}

impl OpenAi<StandardServiceFactory> {
    /// Creates a client with the standard persistent WebSocket and retry stack.
    ///
    /// # Errors
    ///
    /// Returns an error when the supplied credentials are unavailable.
    pub fn new(auth: impl Into<OpenAiAuth>) -> Result<Self, OpenAiError> {
        Self::builder(auth).build()
    }

    /// Starts configuring an `OpenAI` client.
    #[must_use]
    pub fn builder(auth: impl Into<OpenAiAuth>) -> OpenAiBuilder<StandardServiceFactory> {
        let auth = auth.into();
        let mode = auth.mode();
        let mut config = ModelConfig {
            auth,
            ..ModelConfig::default()
        };
        apply_mode_defaults(&mut config, mode);
        OpenAiBuilder {
            config,
            factory: StandardServiceFactory::default(),
        }
    }
}

impl<F> OpenAi<F>
where
    F: MakeResponsesService,
{
    /// Starts a client-side managed session with stable developer
    /// instructions.
    ///
    /// The returned builder does not make a network request. Its `build`
    /// method creates fresh transport and context state.
    #[must_use]
    pub fn instructions(&self, instructions: impl Into<Arc<str>>) -> SessionBuilder<F> {
        SessionBuilder::new(self.clone(), instructions.into())
    }

    pub(crate) fn config(&self) -> &ModelConfig {
        &self.config
    }

    pub(crate) fn make_service(&self) -> F::Service {
        self.factory.make(Arc::new(self.config.clone()))
    }
}

/// Builder for a configured `OpenAI` client and concrete Tower service factory.
#[derive(Clone)]
pub struct OpenAiBuilder<F = StandardServiceFactory> {
    config: ModelConfig,
    factory: F,
}

impl<F> OpenAiBuilder<F> {
    /// Selects the Responses transport used by sessions from this client.
    #[must_use]
    pub const fn transport(mut self, transport: ResponsesTransport) -> Self {
        self.config.responses_transport = transport;
        if matches!(transport, ResponsesTransport::Https) && !self.config.store_responses {
            self.config.responses_history = ResponsesHistory::FullReplay;
        }
        self
    }

    /// Selects incremental continuation or complete replay for healthy calls.
    #[must_use]
    pub const fn history(mut self, history: ResponsesHistory) -> Self {
        self.config.responses_history = history;
        self
    }

    /// Controls whether the provider retains Responses checkpoints.
    #[must_use]
    pub const fn store(mut self, store: bool) -> Self {
        self.config.store_responses = store;
        if !store && matches!(self.config.responses_transport, ResponsesTransport::Https) {
            self.config.responses_history = ResponsesHistory::FullReplay;
        }
        self
    }

    /// Sets the reasoning effort captured by each new response.
    #[must_use]
    pub const fn thinking(mut self, thinking: Thinking) -> Self {
        self.config.thinking = thinking;
        self
    }

    /// Sets the reasoning execution mode.
    #[must_use]
    pub const fn reasoning_mode(mut self, reasoning_mode: ReasoningMode) -> Self {
        self.config.reasoning_mode = reasoning_mode;
        self
    }

    /// Selects priority processing for each new response.
    #[must_use]
    pub const fn fast_mode(mut self, enabled: bool) -> Self {
        self.config.fast_mode = enabled;
        self
    }

    /// Replaces the standard Responses WebSocket endpoint.
    #[must_use]
    pub fn websocket_url(mut self, url: impl Into<String>) -> Self {
        self.config.websocket_url = url.into();
        self
    }

    /// Replaces the standard `OpenAI` API base URL.
    #[must_use]
    pub fn api_base_url(mut self, url: impl Into<String>) -> Self {
        self.config.api_base_url = url.into();
        self
    }

    /// Applies a Tower layer without boxing the resulting service.
    ///
    /// Common Tower middleware that returns [`tower::BoxError`] is converted
    /// into [`crate::ResponseError`] without discarding its source.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use nanocodex_oai_api::OpenAi;
    /// use tower::timeout::TimeoutLayer;
    ///
    /// let openai = OpenAi::builder("test-api-key")
    ///     .layer(TimeoutLayer::new(Duration::from_secs(45)))
    ///     .build()?;
    ///
    /// let session = openai
    ///     .instructions("Preserve exact identifiers and answer concisely.")
    ///     .build()?;
    /// assert_eq!(session.history_len(), 0);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn layer<L>(self, layer: L) -> OpenAiBuilder<LayeredServiceFactory<F, L>> {
        OpenAiBuilder {
            config: self.config,
            factory: LayeredServiceFactory {
                inner: self.factory,
                layer,
            },
        }
    }

    /// Replaces the standard stack with a factory for independent services.
    ///
    /// The factory runs once per managed session. Its service receives a
    /// replayable [`crate::ResponsesAttempt`], may emit normalized streaming
    /// events through [`crate::ResponsesAttempt::emit`], and returns one
    /// complete [`crate::ResponsesServiceResponse`].
    ///
    /// ```no_run
    /// use nanocodex_oai_api::{
    ///     ContentItem, GenerationOutput, MessageRole, OpenAi, ResponseError,
    ///     ResponseEvent, ResponseItem, ResponsePipelineStats, ResponsesAttempt,
    ///     ResponsesOutput, ResponsesServiceResponse,
    /// };
    /// use tower::service_fn;
    ///
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let openai = OpenAi::builder("test-api-key")
    ///     .service(|| {
    ///         service_fn(|request: ResponsesAttempt| async move {
    ///             request
    ///                 .emit(ResponseEvent::OutputTextDelta(
    ///                     "served by the adapter".to_owned(),
    ///                 ))
    ///                 .await;
    ///             let item = ResponseItem::message(
    ///                 MessageRole::Assistant,
    ///                 [ContentItem::output_text("served by the adapter")],
    ///             );
    ///             Ok::<_, ResponseError>(ResponsesServiceResponse::new(
    ///                 ResponsesOutput::Generation(GenerationOutput {
    ///                     id: "resp_adapter_01".to_owned(),
    ///                     status: "completed".to_owned(),
    ///                     end_turn: Some(true),
    ///                     final_message: Some("served by the adapter".to_owned()),
    ///                     output_items: vec![item],
    ///                     code_calls: Vec::new(),
    ///                     usage: None,
    ///                     time_to_first_event_ns: 0,
    ///                     time_to_first_output_ns: Some(0),
    ///                     pipeline_stats: ResponsePipelineStats::default(),
    ///                 }),
    ///             ))
    ///         })
    ///     })
    ///     .build()?;
    /// let mut session = openai
    ///     .instructions("Return the adapter's exact retained result.")
    ///     .build()?;
    ///
    /// let completed = session
    ///     .turn()
    ///     .create("Return the retained result.")
    ///     .await?;
    /// assert_eq!(completed.output_text(), "served by the adapter");
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn service<M>(self, make: M) -> OpenAiBuilder<CallerServiceFactory<M>> {
        OpenAiBuilder {
            config: self.config,
            factory: CallerServiceFactory {
                make: Arc::new(make),
            },
        }
    }
}

impl OpenAiBuilder<StandardServiceFactory> {
    /// Sets the total attempt limit for the standard typed retry policy.
    #[must_use]
    pub const fn max_attempts(mut self, max_attempts: NonZeroU32) -> Self {
        self.factory.max_attempts = max_attempts;
        self
    }

    /// Replaces the HTTP client used by the standard HTTPS/SSE transport.
    #[cfg(not(target_family = "wasm"))]
    #[must_use]
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.factory.http_client = Some(client);
        self
    }
}

impl<F> OpenAiBuilder<F>
where
    F: MakeResponsesService,
{
    /// Validates the configuration and returns a cloneable client recipe.
    ///
    /// # Errors
    ///
    /// Returns an error for unavailable credentials or an incompatible
    /// transport, storage, and replay configuration.
    pub fn build(self) -> Result<OpenAi<F>, OpenAiError> {
        validate(&self.config)?;
        Ok(OpenAi {
            config: self.config,
            factory: self.factory,
        })
    }
}

/// Standard service factory for the persistent transport and typed retry
/// stack.
#[derive(Clone)]
pub struct StandardServiceFactory {
    max_attempts: NonZeroU32,
    #[cfg(not(target_family = "wasm"))]
    http_client: Option<reqwest::Client>,
}

impl Default for StandardServiceFactory {
    fn default() -> Self {
        Self {
            max_attempts: ResponsesRetryPolicy::DEFAULT_MAX_ATTEMPTS,
            #[cfg(not(target_family = "wasm"))]
            http_client: None,
        }
    }
}

/// Caller-supplied fresh-service factory.
#[doc(hidden)]
pub struct CallerServiceFactory<M> {
    make: Arc<M>,
}

impl<M> Clone for CallerServiceFactory<M> {
    fn clone(&self) -> Self {
        Self {
            make: Arc::clone(&self.make),
        }
    }
}

/// A concrete Tower layer applied to another service factory.
#[doc(hidden)]
#[derive(Clone)]
pub struct LayeredServiceFactory<F, L> {
    inner: F,
    layer: L,
}

/// Private generic construction boundary used to retain concrete Tower types.
#[doc(hidden)]
pub trait MakeResponsesService: Clone {
    /// Concrete service owned by each managed session.
    type Service;

    /// Creates one independent service stack.
    fn make(&self, config: Arc<ModelConfig>) -> Self::Service;
}

impl MakeResponsesService for StandardServiceFactory {
    type Service = DefaultResponsesService;

    fn make(&self, config: Arc<ModelConfig>) -> Self::Service {
        #[cfg(not(target_family = "wasm"))]
        {
            self.http_client.as_ref().map_or_else(
                || {
                    ResponsesService::standard_with_max_attempts(
                        Arc::clone(&config),
                        self.max_attempts,
                    )
                },
                |client| {
                    ResponsesService::standard_with_http_client_and_max_attempts(
                        Arc::clone(&config),
                        client.clone(),
                        self.max_attempts,
                    )
                },
            )
        }
        #[cfg(target_family = "wasm")]
        {
            ResponsesService::standard_with_max_attempts(config, self.max_attempts)
        }
    }
}

impl<M, S> MakeResponsesService for CallerServiceFactory<M>
where
    M: Fn() -> S,
{
    type Service = S;

    fn make(&self, _config: Arc<ModelConfig>) -> Self::Service {
        (self.make)()
    }
}

impl<F, L> MakeResponsesService for LayeredServiceFactory<F, L>
where
    F: MakeResponsesService,
    L: Layer<F::Service> + Clone,
{
    type Service = L::Service;

    fn make(&self, config: Arc<ModelConfig>) -> Self::Service {
        self.layer.layer(self.inner.make(config))
    }
}

/// Invalid `OpenAI` client configuration.
#[derive(Debug, thiserror::Error)]
pub enum OpenAiError {
    /// Credentials cannot currently provide an authorization value.
    #[error(transparent)]
    Authorization(#[from] OpenAiAuthError),
    /// Two client policies cannot be satisfied together.
    #[error("invalid OpenAI client configuration: {detail}")]
    InvalidConfiguration {
        /// Human-readable explanation without credentials.
        detail: &'static str,
    },
}

fn apply_mode_defaults(config: &mut ModelConfig, mode: OpenAiAuthMode) {
    config.store_responses = mode.supports_stored_responses();
    config.responses_history = ResponsesHistory::Incremental;
    mode.default_websocket_url()
        .clone_into(&mut config.websocket_url);
    mode.default_api_base_url()
        .clone_into(&mut config.api_base_url);
}

fn validate(config: &ModelConfig) -> Result<(), OpenAiError> {
    config.auth.validate()?;
    if config.websocket_url.trim().is_empty() {
        return Err(OpenAiError::InvalidConfiguration {
            detail: "the Responses WebSocket URL must not be empty",
        });
    }
    if config.api_base_url.trim().is_empty() {
        return Err(OpenAiError::InvalidConfiguration {
            detail: "the OpenAI API base URL must not be empty",
        });
    }
    if config.auth.mode() == OpenAiAuthMode::ChatGpt && config.store_responses {
        return Err(OpenAiError::InvalidConfiguration {
            detail: "ChatGPT subscription authentication does not support stored responses",
        });
    }
    if matches!(config.responses_transport, ResponsesTransport::Https)
        && !config.store_responses
        && matches!(config.responses_history, ResponsesHistory::Incremental)
    {
        return Err(OpenAiError::InvalidConfiguration {
            detail: "HTTPS without stored responses requires complete client-history replay",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        error::Error as _,
        future::{Ready, pending},
        time::Duration,
    };

    use tower::{Service, service_fn, timeout::TimeoutLayer};

    use crate::{ResponseError, ResponsesAttempt, ResponsesServiceResponse};

    use super::OpenAi;

    #[derive(Clone)]
    struct NeverCalled;

    impl Service<ResponsesAttempt> for NeverCalled {
        type Response = ResponsesServiceResponse;
        type Error = Infallible;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
            panic!("test service should not be called")
        }
    }

    #[test]
    fn one_client_recipe_builds_independent_sessions() {
        let client = OpenAi::builder("test-key")
            .service(|| NeverCalled)
            .build()
            .unwrap();

        let session = client.instructions("Answer only from supplied facts.");
        let first = session.clone().build().unwrap();
        let second = session.build().unwrap();

        assert_ne!(first.id(), second.id());
    }

    #[tokio::test]
    async fn tower_box_errors_remain_usable_through_the_managed_response_api() {
        let client = OpenAi::builder("test-key")
            .service(|| {
                service_fn(|_request: ResponsesAttempt| {
                    pending::<Result<ResponsesServiceResponse, ResponseError>>()
                })
            })
            .layer(TimeoutLayer::new(Duration::from_millis(1)))
            .build()
            .unwrap();
        let mut session = client
            .instructions("Return exactly one short answer.")
            .build()
            .unwrap();

        let error = session
            .turn()
            .create("This request should reach the test deadline.")
            .await
            .unwrap_err();

        assert!(error.source().is_some());
        assert!(error.to_string().contains("request timed out"));
    }
}
