use std::{
    convert::Infallible,
    error::Error,
    fmt,
    future::{Future, IntoFuture},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::{Stream, future::poll_fn};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tower::Service;

use crate::{
    ContentItem, EventSink, MessageRole, OpenAi, ResponseEvent, ResponseItem, ResponsesAttempt,
    ResponsesAttemptFactory, ResponsesClient, ResponsesOutput, ResponsesServiceError,
    ResponsesServiceResponse, Thinking, ToolDefinition, TransportStats, Usage, compaction,
    context::{ContextManager, assign_missing_response_item_id},
    openai::{MakeResponsesService, StandardServiceFactory},
    responses::{RequestProfile, ResponseHistory},
};

const RESPONSE_EVENT_CAPACITY: usize = 64;

/// Stable client-owned identity for one managed conversation.
///
/// Session IDs are `UUIDv7` values so they remain globally unique while sorting
/// by creation time. They are not `OpenAI` response IDs and are safe to persist
/// as application lineage.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(try_from = "uuid::Uuid", into = "uuid::Uuid")]
pub struct SessionId(uuid::Uuid);

impl SessionId {
    /// Generates a new `UUIDv7` session identity.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// Returns the UUID representation.
    #[must_use]
    pub const fn as_uuid(self) -> uuid::Uuid {
        self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("SessionId").field(&self.0).finish()
    }
}

impl std::str::FromStr for SessionId {
    type Err = SessionIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(uuid::Uuid::parse_str(value)?)
    }
}

impl TryFrom<uuid::Uuid> for SessionId {
    type Error = SessionIdError;

    fn try_from(value: uuid::Uuid) -> Result<Self, Self::Error> {
        if value.get_version_num() != 7 {
            return Err(SessionIdError::WrongVersion {
                version: value.get_version_num(),
            });
        }
        Ok(Self(value))
    }
}

impl From<SessionId> for uuid::Uuid {
    fn from(value: SessionId) -> Self {
        value.0
    }
}

/// Invalid persisted or caller-supplied session identity.
#[derive(Debug, thiserror::Error)]
pub enum SessionIdError {
    /// The value was not a UUID.
    #[error("invalid session UUID")]
    InvalidUuid(#[from] uuid::Error),
    /// The UUID used a version other than `UUIDv7`.
    #[error("session IDs must be UUIDv7, got UUIDv{version}")]
    WrongVersion {
        /// Parsed UUID version number.
        version: usize,
    },
}

/// Builder for one instruction-bound managed Responses session.
#[derive(Clone)]
pub struct SessionBuilder<F = StandardServiceFactory> {
    openai: OpenAi<F>,
    instructions: Arc<str>,
    session_id: Option<SessionId>,
    prompt_cache_key: Option<String>,
    tools: Vec<ToolDefinition>,
}

impl<F> SessionBuilder<F>
where
    F: MakeResponsesService,
{
    pub(crate) fn new(openai: OpenAi<F>, instructions: Arc<str>) -> Self {
        Self {
            openai,
            instructions,
            session_id: None,
            prompt_cache_key: None,
            tools: Vec::new(),
        }
    }

    /// Sets the client-side session identity used for tracing and cache
    /// lineage.
    #[must_use]
    pub const fn session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// Sets the stable cache key for the immutable instructions and tools.
    #[must_use]
    pub fn prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(prompt_cache_key.into());
        self
    }

    /// Installs complete typed tool definitions.
    ///
    /// This low-level constructor exists for transport implementers. Normal
    /// callers register `Tool` implementations through `nanocodex-tools`.
    #[doc(hidden)]
    #[must_use]
    pub fn tool_definitions(mut self, tools: impl IntoIterator<Item = ToolDefinition>) -> Self {
        self.tools = tools.into_iter().collect();
        self
    }

    /// Creates fresh service, context, and continuation state.
    ///
    /// # Errors
    ///
    /// Returns an error when instructions or the cache identity are empty.
    pub fn build(self) -> Result<Session<F::Service>, SessionBuildError> {
        if self.instructions.trim().is_empty() {
            return Err(SessionBuildError::EmptyInstructions);
        }
        let session_id = self.session_id.unwrap_or_default();
        let prompt_cache_key = self
            .prompt_cache_key
            .unwrap_or_else(|| session_id.to_string());
        if prompt_cache_key.trim().is_empty() {
            return Err(SessionBuildError::EmptyPromptCacheKey);
        }

        let mut prefix = [
            ResponseItem::additional_tools(self.tools),
            ResponseItem::message(
                MessageRole::Developer,
                [ContentItem::InputText {
                    text: self.instructions.to_string().into_boxed_str(),
                }],
            ),
        ];
        assign_request_prefix_ids(&mut prefix);
        let profile =
            RequestProfile::new(session_id.to_string(), prompt_cache_key, Arc::from(prefix));
        let config = self.openai.config().clone();
        let service = self.openai.make_service();

        Ok(Session {
            id: session_id,
            client: ResponsesClient::new(service),
            profile,
            context: ContextManager::new(Vec::new()),
            previous_response_id: None,
            delta_start: 0,
            next_call_index: 1,
            next_logical_turn: 1,
            thinking: config.thinking,
            fast_mode: config.fast_mode,
            server_reasoning_included: false,
            transport_stats: Arc::new(TransportStats::default()),
        })
    }
}

/// Invalid managed-session construction.
#[derive(Debug, thiserror::Error)]
pub enum SessionBuildError {
    /// Stable developer instructions were empty.
    #[error("OpenAI session instructions must not be empty")]
    EmptyInstructions,
    /// The explicit prompt-cache identity was empty.
    #[error("OpenAI prompt cache key must not be empty")]
    EmptyPromptCacheKey,
}

/// One managed `OpenAI` Responses conversation.
///
/// The session owns its concrete Tower service, persistent transport,
/// authoritative typed history, usage, and private continuation state.
pub struct Session<S> {
    id: SessionId,
    client: ResponsesClient<S>,
    profile: RequestProfile,
    context: ContextManager,
    previous_response_id: Option<String>,
    delta_start: usize,
    next_call_index: u32,
    next_logical_turn: u64,
    thinking: Thinking,
    fast_mode: bool,
    server_reasoning_included: bool,
    transport_stats: Arc<TransportStats>,
}

impl<S> Session<S> {
    /// Returns the client-side session identity.
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// Starts one logical agent turn.
    ///
    /// Every `create` and `compact` call made through the returned value shares
    /// turn-scoped protocol state. Dropping it ends the boundary.
    pub fn turn(&mut self) -> ResponseTurn<'_, S> {
        let logical_turn = self.next_logical_turn;
        self.next_logical_turn = self.next_logical_turn.saturating_add(1);
        ResponseTurn {
            session: self,
            logical_turn,
        }
    }

    /// Returns the number of committed typed history items.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.context.len()
    }

    /// Iterates over committed authoritative history without exposing mutable
    /// access.
    pub fn history(&self) -> impl ExactSizeIterator<Item = &ResponseItem> {
        self.context.iter()
    }

    /// Returns the best available estimate of tokens in the active provider
    /// context.
    ///
    /// The session combines completed provider usage with locally appended
    /// items and retained reasoning according to the transport metadata it has
    /// observed. Higher-level agents use this summary to decide *when* to call
    /// [`ResponseTurn::compact`].
    #[must_use]
    pub fn active_context_tokens(&self) -> u64 {
        self.context
            .active_context_tokens(self.server_reasoning_included)
    }
}

/// Turn-scoped Responses operations borrowing one managed session.
pub struct ResponseTurn<'session, S> {
    session: &'session mut Session<S>,
    logical_turn: u64,
}

impl<S> ResponseTurn<'_, S> {
    /// Returns the session's best available active-context token estimate.
    #[must_use]
    pub fn active_context_tokens(&self) -> u64 {
        self.session.active_context_tokens()
    }
}

#[cfg(not(target_family = "wasm"))]
impl<S> ResponseTurn<'_, S>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + Send,
    S::Error: Into<ResponseError> + Send,
    S::Future: Send,
{
    /// Starts one streamed `response.create` operation.
    pub fn create(&mut self, input: impl Into<ResponseInput>) -> Response<'_> {
        let (sink, raw_events) = EventSink::channel(self.session.profile.session_id().to_owned());
        drop(raw_events);
        let (response_events, events) = mpsc::channel(RESPONSE_EVENT_CAPACITY);
        let run = Box::pin(run_create(self, input.into(), sink, response_events));
        Response::new(events, run)
    }

    /// Executes `response.compact` and atomically installs its completed
    /// history replacement.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, protocol, or context error. Failed
    /// compaction leaves the prior history untouched.
    pub async fn compact(&mut self) -> Result<CompletedCompaction, ResponseError> {
        run_compact(self).await
    }
}

#[cfg(target_family = "wasm")]
impl<S> ResponseTurn<'_, S>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse>,
    S::Error: Into<ResponseError>,
{
    /// Starts one streamed `response.create` operation.
    pub fn create(&mut self, input: impl Into<ResponseInput>) -> Response<'_> {
        let (sink, raw_events) = EventSink::channel(self.session.profile.session_id().to_owned());
        drop(raw_events);
        let (response_events, events) = mpsc::channel(RESPONSE_EVENT_CAPACITY);
        let run = Box::pin(run_create(self, input.into(), sink, response_events));
        Response::new(events, run)
    }

    /// Executes `response.compact` and atomically installs its completed
    /// history replacement.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, protocol, or context error. Failed
    /// compaction leaves the prior history untouched.
    pub async fn compact(&mut self) -> Result<CompletedCompaction, ResponseError> {
        run_compact(self).await
    }
}

/// Typed input accepted by `response.create`.
#[derive(Clone, Debug)]
pub struct ResponseInput {
    items: Vec<ResponseItem>,
}

impl ResponseInput {
    /// Creates one user message from ordered text, image, or audio content.
    ///
    /// ```
    /// use nanocodex_oai_api::{ContentItem, ImageDetail, ResponseInput};
    ///
    /// let _input = ResponseInput::content([
    ///     ContentItem::input_text("Describe the deployment diagram."),
    ///     ContentItem::input_image_with_detail(
    ///         "https://example.com/deployment-diagram.png",
    ///         ImageDetail::High,
    ///     ),
    /// ]);
    /// ```
    #[must_use]
    pub fn content(content: impl IntoIterator<Item = ContentItem>) -> Self {
        Self {
            items: vec![ResponseItem::message(MessageRole::User, content)],
        }
    }

    /// Estimates model-visible tokens contributed by this input.
    ///
    /// This uses the same image, text, and tool-output accounting as managed
    /// context. A caller can combine it with
    /// [`ResponseTurn::active_context_tokens`] and choose its own compaction
    /// margin before calling [`ResponseTurn::create`].
    #[must_use]
    pub fn estimated_tokens(&self) -> u64 {
        self.items
            .iter()
            .map(compaction::estimate_item_tokens)
            .fold(0, u64::saturating_add)
    }

    /// Creates low-level typed input items.
    ///
    /// The agent and tools crates use this for paired tool outputs. General
    /// callers should prefer text conversion or `content`.
    #[doc(hidden)]
    #[must_use]
    pub fn items(items: impl IntoIterator<Item = ResponseItem>) -> Self {
        Self {
            items: items.into_iter().collect(),
        }
    }
}

impl From<String> for ResponseInput {
    fn from(text: String) -> Self {
        Self::content([ContentItem::InputText {
            text: text.into_boxed_str(),
        }])
    }
}

impl From<&str> for ResponseInput {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

/// A completed and atomically committed Responses operation.
#[derive(Clone)]
pub struct CompletedResponse {
    output: Arc<[ResponseItem]>,
    output_text: Arc<str>,
    usage: Option<Usage>,
    end_turn: Option<bool>,
    checkpoint: ResponseCheckpoint,
}

impl CompletedResponse {
    /// Returns every completed output item in provider order.
    #[must_use]
    pub fn output(&self) -> &[ResponseItem] {
        &self.output
    }

    /// Returns concatenated assistant output text.
    #[must_use]
    pub fn output_text(&self) -> &str {
        &self.output_text
    }

    /// Iterates over complete function and custom tool calls.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ResponseItem> {
        self.output.iter().filter(|item| {
            matches!(
                item,
                ResponseItem::FunctionCall { .. }
                    | ResponseItem::CustomToolCall { .. }
                    | ResponseItem::LocalShellCall { .. }
                    | ResponseItem::ToolSearchCall { .. }
            )
        })
    }

    /// Returns token usage for this API operation.
    #[must_use]
    pub const fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    /// Returns whether the model affirmatively ended its logical turn.
    #[must_use]
    pub const fn end_turn(&self) -> Option<bool> {
        self.end_turn
    }

    /// Returns the opaque completed client-owned checkpoint.
    #[must_use]
    pub const fn checkpoint(&self) -> &ResponseCheckpoint {
        &self.checkpoint
    }
}

impl fmt::Debug for CompletedResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletedResponse")
            .field("output_items", &self.output.len())
            .field("output_text", &self.output_text)
            .field("end_turn", &self.end_turn)
            .finish_non_exhaustive()
    }
}

/// Opaque completed session boundary.
#[derive(Clone)]
pub struct ResponseCheckpoint {
    session_id: SessionId,
    history: ResponseHistory,
}

impl fmt::Debug for ResponseCheckpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponseCheckpoint")
            .field("session_id", &self.session_id)
            .field("history_items", &self.history.len())
            .finish_non_exhaustive()
    }
}

/// A completed and installed remote compaction.
#[derive(Clone)]
pub struct CompletedCompaction {
    usage: Option<Usage>,
    checkpoint: ResponseCheckpoint,
}

impl CompletedCompaction {
    /// Returns token usage reported by the compaction operation.
    #[must_use]
    pub const fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    /// Returns the opaque completed client-owned checkpoint.
    #[must_use]
    pub const fn checkpoint(&self) -> &ResponseCheckpoint {
        &self.checkpoint
    }
}

#[cfg(not(target_family = "wasm"))]
type ResponseRun<'a> =
    Pin<Box<dyn Future<Output = Result<CompletedResponse, ResponseError>> + Send + 'a>>;
#[cfg(target_family = "wasm")]
type ResponseRun<'a> = Pin<Box<dyn Future<Output = Result<CompletedResponse, ResponseError>> + 'a>>;

/// A single Responses operation that is both a typed stream and an awaitable
/// completed aggregate.
#[must_use = "a response does no work unless it is streamed or awaited"]
pub struct Response<'a> {
    events: mpsc::Receiver<ResponseEvent>,
    run: ResponseRun<'a>,
    result: Option<Result<CompletedResponse, ResponseError>>,
    run_finished: bool,
    completed_event_seen: bool,
    stream_error_emitted: bool,
}

impl<'a> Response<'a> {
    fn new(events: mpsc::Receiver<ResponseEvent>, run: ResponseRun<'a>) -> Self {
        Self {
            events,
            run,
            result: None,
            run_finished: false,
            completed_event_seen: false,
            stream_error_emitted: false,
        }
    }
}

impl Stream for Response<'_> {
    type Item = Result<ResponseEvent, ResponseError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.events.poll_recv(context) {
                Poll::Ready(Some(event)) => {
                    if matches!(event, ResponseEvent::Completed { .. }) {
                        self.completed_event_seen = true;
                    }
                    return Poll::Ready(Some(Ok(event)));
                }
                Poll::Ready(None) | Poll::Pending => {}
            }

            if !self.run_finished {
                match self.run.as_mut().poll(context) {
                    Poll::Ready(result) => {
                        self.result = Some(result);
                        self.run_finished = true;
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            if !self.completed_event_seen
                && let Some(Ok(result)) = self.result.as_ref()
            {
                let event = ResponseEvent::Completed {
                    usage: result.usage.clone(),
                    end_turn: result.end_turn,
                };
                self.completed_event_seen = true;
                return Poll::Ready(Some(Ok(event)));
            }

            let stream_error = (!self.stream_error_emitted)
                .then(|| self.result.as_ref())
                .flatten()
                .and_then(|result| result.as_ref().err())
                .cloned();
            if let Some(error) = stream_error {
                self.stream_error_emitted = true;
                return Poll::Ready(Some(Err(error)));
            }
            return Poll::Ready(None);
        }
    }
}

#[cfg(not(target_family = "wasm"))]
type ResponseIntoFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompletedResponse, ResponseError>> + Send + 'a>>;
#[cfg(target_family = "wasm")]
type ResponseIntoFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompletedResponse, ResponseError>> + 'a>>;

impl<'a> IntoFuture for Response<'a> {
    type Output = Result<CompletedResponse, ResponseError>;
    type IntoFuture = ResponseIntoFuture<'a>;

    fn into_future(mut self) -> Self::IntoFuture {
        Box::pin(async move {
            while poll_fn(|context| Pin::new(&mut self).poll_next(context))
                .await
                .is_some()
            {}
            self.result.take().unwrap_or_else(|| {
                Err(ResponseError::protocol(
                    "response stream ended without a terminal service result",
                ))
            })
        })
    }
}

/// Cloneable typed failure returned by a response stream and its completed
/// future.
#[derive(Clone)]
pub struct ResponseError {
    kind: Arc<ResponseErrorKind>,
}

enum ResponseErrorKind {
    ContextWindowExceeded(Arc<dyn Error + Send + Sync>),
    Service(Arc<dyn Error + Send + Sync>),
    Protocol(Arc<str>),
}

impl ResponseError {
    /// Wraps a caller-composed Tower service error without erasing its source.
    #[must_use]
    pub fn service(error: impl Error + Send + Sync + 'static) -> Self {
        let context_window_exceeded = error_chain_has_context_window(&error);
        let error: Arc<dyn Error + Send + Sync> = Arc::new(error);
        Self {
            kind: Arc::new(if context_window_exceeded {
                ResponseErrorKind::ContextWindowExceeded(error)
            } else {
                ResponseErrorKind::Service(error)
            }),
        }
    }

    fn protocol(detail: impl Into<Arc<str>>) -> Self {
        Self {
            kind: Arc::new(ResponseErrorKind::Protocol(detail.into())),
        }
    }

    /// Returns whether the provider rejected the request for context-window
    /// exhaustion.
    #[must_use]
    pub fn is_context_window_exceeded(&self) -> bool {
        matches!(
            self.kind.as_ref(),
            ResponseErrorKind::ContextWindowExceeded(_)
        )
    }
}

impl From<ResponsesServiceError> for ResponseError {
    fn from(error: ResponsesServiceError) -> Self {
        let context_window_exceeded = error.is_context_window_exceeded();
        let error: Arc<dyn Error + Send + Sync> = Arc::new(error);
        let kind = if context_window_exceeded {
            ResponseErrorKind::ContextWindowExceeded(error)
        } else {
            ResponseErrorKind::Service(error)
        };
        Self {
            kind: Arc::new(kind),
        }
    }
}

impl From<tower::BoxError> for ResponseError {
    fn from(error: tower::BoxError) -> Self {
        let context_window_exceeded = error_chain_has_context_window(error.as_ref());
        let error: Arc<dyn Error + Send + Sync> = Arc::from(error);
        Self {
            kind: Arc::new(if context_window_exceeded {
                ResponseErrorKind::ContextWindowExceeded(error)
            } else {
                ResponseErrorKind::Service(error)
            }),
        }
    }
}

impl From<Infallible> for ResponseError {
    fn from(error: Infallible) -> Self {
        match error {}
    }
}

impl fmt::Display for ResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind.as_ref() {
            ResponseErrorKind::ContextWindowExceeded(_) => {
                formatter.write_str("Responses input exceeded the model context window")
            }
            ResponseErrorKind::Service(error) => error.fmt(formatter),
            ResponseErrorKind::Protocol(detail) => {
                write!(formatter, "invalid Responses state: {detail}")
            }
        }
    }
}

impl fmt::Debug for ResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponseError")
            .field("message", &self.to_string())
            .finish_non_exhaustive()
    }
}

impl Error for ResponseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self.kind.as_ref() {
            ResponseErrorKind::ContextWindowExceeded(error) | ResponseErrorKind::Service(error) => {
                Some(error.as_ref())
            }
            ResponseErrorKind::Protocol(_) => None,
        }
    }
}

fn error_chain_has_context_window(mut error: &(dyn Error + 'static)) -> bool {
    loop {
        if error
            .downcast_ref::<ResponsesServiceError>()
            .is_some_and(ResponsesServiceError::is_context_window_exceeded)
        {
            return true;
        }
        let Some(source) = error.source() else {
            return false;
        };
        error = source;
    }
}

async fn run_create<S>(
    turn: &mut ResponseTurn<'_, S>,
    input: ResponseInput,
    sink: EventSink,
    response_events: mpsc::Sender<ResponseEvent>,
) -> Result<CompletedResponse, ResponseError>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse>,
    S::Error: Into<ResponseError>,
{
    if input.items.is_empty() {
        return Err(ResponseError::protocol(
            "response.create input must contain at least one item",
        ));
    }

    let session = &mut *turn.session;
    let call_index = session.next_call_index;
    session.next_call_index = session.next_call_index.saturating_add(1);
    let mut candidate = session.context.clone();
    candidate.record_items(input.items);

    let factory = ResponsesAttemptFactory::new(
        session.profile.clone(),
        sink,
        Arc::clone(&session.transport_stats),
    )
    .with_response_events(response_events)
    .for_logical_turn(turn.logical_turn);
    let request = factory.generation(
        call_index,
        candidate.prompt_items(),
        candidate.shared_items(),
        session.delta_start,
        session.previous_response_id.as_deref(),
        session.thinking,
        session.fast_mode,
    );
    let success = session.client.execute(request).await.map_err(Into::into)?;
    session.server_reasoning_included |= success.server_reasoning_included();
    let ResponsesOutput::Generation(response) = success.into_output() else {
        return Err(ResponseError::protocol(
            "response.create returned a non-generation output",
        ));
    };

    candidate.record_items(response.output_items.clone());
    candidate.update_token_info(response.usage.as_ref());
    candidate.commit_tail();
    session.context = candidate;
    session.delta_start = session.context.len();
    session.previous_response_id = Some(response.id);

    let output: Arc<[ResponseItem]> = response.output_items.into();
    let output_text = response
        .final_message
        .unwrap_or_else(|| output_text(&output))
        .into();
    Ok(CompletedResponse {
        output,
        output_text,
        usage: response.usage,
        end_turn: response.end_turn,
        checkpoint: session.checkpoint(),
    })
}

async fn run_compact<S>(
    turn: &mut ResponseTurn<'_, S>,
) -> Result<CompletedCompaction, ResponseError>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse>,
    S::Error: Into<ResponseError>,
{
    let session = &mut *turn.session;
    let call_index = session.next_call_index;
    session.next_call_index = session.next_call_index.saturating_add(1);
    let (sink, events) = EventSink::channel(session.profile.session_id().to_owned());
    drop(events);
    let factory = ResponsesAttemptFactory::new(
        session.profile.clone(),
        sink,
        Arc::clone(&session.transport_stats),
    )
    .for_logical_turn(turn.logical_turn);
    let mut history = session.context.prompt_items();
    compaction::trim_tool_outputs_to_fit_context_window(&mut history, session.profile.prefix());
    let request = factory.compaction(
        call_index,
        history.clone(),
        history,
        session.delta_start,
        session.previous_response_id.as_deref(),
        compaction::trigger(),
        session.thinking,
        session.fast_mode,
    );
    let success = session.client.execute(request).await.map_err(Into::into)?;
    session.server_reasoning_included |= success.server_reasoning_included();
    let ResponsesOutput::Compaction(response) = success.into_output() else {
        return Err(ResponseError::protocol(
            "response.compact returned a non-compaction output",
        ));
    };

    let installed =
        compaction::install_history(&session.context.flattened_items(), &[], response.item);
    session
        .context
        .replace_and_recompute(installed, session.profile.prefix());
    session.delta_start = 0;
    session.previous_response_id = None;

    Ok(CompletedCompaction {
        usage: response.usage,
        checkpoint: session.checkpoint(),
    })
}

impl<S> Session<S> {
    fn checkpoint(&self) -> ResponseCheckpoint {
        ResponseCheckpoint {
            session_id: self.id,
            history: self.context.shared_items(),
        }
    }
}

fn output_text(items: &[ResponseItem]) -> String {
    items
        .iter()
        .filter_map(|item| {
            let ResponseItem::Message { content, .. } = item else {
                return None;
            };
            Some(content.iter().filter_map(|content| {
                let ContentItem::OutputText { text, .. } = content else {
                    return None;
                };
                Some(text.as_ref())
            }))
        })
        .flatten()
        .collect()
}

fn assign_request_prefix_ids(prefix: &mut [ResponseItem]) {
    for item in prefix {
        if matches!(item, ResponseItem::AdditionalTools { .. }) {
            item.strip_id();
            continue;
        }
        if matches!(
            item,
            ResponseItem::Message {
                role: MessageRole::Developer,
                ..
            }
        ) {
            item.set_id(Some(crate::ResponseItemId::with_suffix(
                "msg",
                "nanocodex-instructions",
            )));
        } else {
            assign_missing_response_item_id(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error as _,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, Ordering},
        },
        task::Poll,
    };

    use futures_util::TryStreamExt;
    use tower::Service;

    use crate::{
        CompactionOutput, ContentItem, GenerationOutput, MessageRole, ResponsePipelineStats,
        ResponsesAttemptKind, ResponsesOutput, ResponsesServiceResponse, SessionId,
    };

    use super::{OpenAi, ResponseError, ResponseEvent};

    #[derive(Clone)]
    struct Scripted {
        calls: Arc<AtomicU32>,
    }

    impl Service<crate::ResponsesAttempt> for Scripted {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(
            &mut self,
            _context: &mut std::task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: crate::ResponsesAttempt) -> Self::Future {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            let item = crate::ResponseItem::message(
                MessageRole::Assistant,
                [ContentItem::OutputText {
                    text: format!("answer-{call}").into(),
                    annotations: None,
                    logprobs: None,
                }],
            );
            Box::pin(async move {
                request
                    .emit(ResponseEvent::OutputTextDelta(format!("answer-{call}")))
                    .await;
                Ok(
                    ResponsesServiceResponse::new(ResponsesOutput::Generation(GenerationOutput {
                        id: format!("resp-{call}"),
                        status: "completed".to_owned(),
                        end_turn: None,
                        final_message: Some(format!("answer-{call}")),
                        output_items: vec![item],
                        code_calls: Vec::new(),
                        usage: Some(crate::Usage {
                            input_tokens: 12,
                            output_tokens: 5,
                            total_tokens: 17,
                            ..crate::Usage::default()
                        }),
                        time_to_first_event_ns: 1,
                        time_to_first_output_ns: Some(1),
                        pipeline_stats: ResponsePipelineStats::default(),
                    }))
                    .with_connection_generation(1)
                    .with_server_reasoning_included(true),
                )
            })
        }
    }

    #[tokio::test]
    async fn response_stream_and_future_share_one_completed_operation() {
        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);
        let openai = OpenAi::builder("test-key")
            .service(move || Scripted {
                calls: Arc::clone(&factory_calls),
            })
            .build()
            .unwrap();
        let mut session = openai
            .instructions("Answer only from supplied facts.")
            .build()
            .unwrap();
        let completed = {
            let mut turn = session.turn();
            let mut response = turn.create("The region is us-west-2.");

            let event = response.try_next().await.unwrap().unwrap();
            assert!(matches!(event, ResponseEvent::OutputTextDelta(delta) if delta == "answer-1"));
            let event = response.try_next().await.unwrap().unwrap();
            assert!(matches!(event, ResponseEvent::Completed { .. }));
            assert!(response.try_next().await.unwrap().is_none());
            response.await.unwrap()
        };

        assert_eq!(completed.output_text(), "answer-1");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(session.history_len(), 2);
        assert_eq!(session.active_context_tokens(), 17);
    }

    #[derive(Debug)]
    struct AttemptObservation {
        previous_response_id: Option<String>,
        full_replay: bool,
        input: Vec<serde_json::Value>,
    }

    #[derive(Clone)]
    struct RecordingScripted {
        calls: Arc<AtomicU32>,
        observations: Arc<Mutex<Vec<AttemptObservation>>>,
    }

    impl Service<crate::ResponsesAttempt> for RecordingScripted {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _context: &mut std::task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: crate::ResponsesAttempt) -> Self::Future {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            self.observations.lock().unwrap().push(AttemptObservation {
                previous_response_id: request.previous_response_id().map(str::to_owned),
                full_replay: request.is_full_replay(),
                input: request
                    .input_items()
                    .map(|item| serde_json::to_value(item).unwrap())
                    .collect(),
            });
            let item = crate::ResponseItem::message(
                MessageRole::Assistant,
                [ContentItem::OutputText {
                    text: format!("answer-{call}").into(),
                    annotations: None,
                    logprobs: None,
                }],
            );
            std::future::ready(Ok(ResponsesServiceResponse::new(
                ResponsesOutput::Generation(GenerationOutput {
                    id: format!("resp-{call}"),
                    status: "completed".to_owned(),
                    end_turn: Some(call == 2),
                    final_message: Some(format!("answer-{call}")),
                    output_items: vec![item],
                    code_calls: Vec::new(),
                    usage: None,
                    time_to_first_event_ns: 1,
                    time_to_first_output_ns: Some(1),
                    pipeline_stats: ResponsePipelineStats::default(),
                }),
            )))
        }
    }

    #[tokio::test]
    async fn sequential_creates_send_only_the_new_delta_after_completion() {
        let calls = Arc::new(AtomicU32::new(0));
        let observations = Arc::new(Mutex::new(Vec::new()));
        let factory_calls = Arc::clone(&calls);
        let factory_observations = Arc::clone(&observations);
        let openai = OpenAi::builder("test-key")
            .service(move || RecordingScripted {
                calls: Arc::clone(&factory_calls),
                observations: Arc::clone(&factory_observations),
            })
            .build()
            .unwrap();
        let mut session = openai
            .instructions("Remember deployment facts between calls.")
            .build()
            .unwrap();

        {
            let mut turn = session.turn();
            assert_eq!(
                turn.create("The region is us-west-2.")
                    .await
                    .unwrap()
                    .output_text(),
                "answer-1"
            );
            assert_eq!(
                turn.create("What region did I give you?")
                    .await
                    .unwrap()
                    .output_text(),
                "answer-2"
            );
        }

        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 2);
        assert!(observations[0].full_replay);
        assert_eq!(observations[0].previous_response_id, None);
        assert_eq!(observations[0].input.len(), 3);
        assert!(!observations[1].full_replay);
        assert_eq!(
            observations[1].previous_response_id.as_deref(),
            Some("resp-1")
        );
        assert_eq!(observations[1].input.len(), 1);
        assert_eq!(observations[1].input[0]["role"], "user");
        assert_eq!(session.history_len(), 4);
    }

    #[derive(Clone)]
    struct CompactingScripted {
        calls: Arc<AtomicU32>,
        observations: Arc<Mutex<Vec<AttemptObservation>>>,
    }

    impl Service<crate::ResponsesAttempt> for CompactingScripted {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _context: &mut std::task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: crate::ResponsesAttempt) -> Self::Future {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            self.observations.lock().unwrap().push(AttemptObservation {
                previous_response_id: request.previous_response_id().map(str::to_owned),
                full_replay: request.is_full_replay(),
                input: request
                    .input_items()
                    .map(|item| serde_json::to_value(item).unwrap())
                    .collect(),
            });
            let output = if matches!(request.kind(), ResponsesAttemptKind::Compaction) {
                ResponsesOutput::Compaction(CompactionOutput {
                    id: format!("resp-{call}"),
                    status: "completed".to_owned(),
                    item: crate::ResponseItem::Compaction {
                        id: None,
                        encrypted_content: "encrypted-summary".into(),
                        created_by: None,
                        internal_chat_message_metadata_passthrough: None,
                    },
                    usage: None,
                    time_to_first_event_ns: 1,
                    time_to_first_output_ns: Some(1),
                    pipeline_stats: ResponsePipelineStats::default(),
                })
            } else {
                let item = crate::ResponseItem::message(
                    MessageRole::Assistant,
                    [ContentItem::OutputText {
                        text: format!("answer-{call}").into(),
                        annotations: None,
                        logprobs: None,
                    }],
                );
                ResponsesOutput::Generation(GenerationOutput {
                    id: format!("resp-{call}"),
                    status: "completed".to_owned(),
                    end_turn: None,
                    final_message: Some(format!("answer-{call}")),
                    output_items: vec![item],
                    code_calls: Vec::new(),
                    usage: None,
                    time_to_first_event_ns: 1,
                    time_to_first_output_ns: Some(1),
                    pipeline_stats: ResponsePipelineStats::default(),
                })
            };
            std::future::ready(Ok(ResponsesServiceResponse::new(output)))
        }
    }

    #[tokio::test]
    async fn compaction_atomically_replaces_history_and_forces_one_full_replay() {
        let calls = Arc::new(AtomicU32::new(0));
        let observations = Arc::new(Mutex::new(Vec::new()));
        let factory_calls = Arc::clone(&calls);
        let factory_observations = Arc::clone(&observations);
        let openai = OpenAi::builder("test-key")
            .service(move || CompactingScripted {
                calls: Arc::clone(&factory_calls),
                observations: Arc::clone(&factory_observations),
            })
            .build()
            .unwrap();
        let mut session = openai
            .instructions("Retain user facts across explicit compaction.")
            .build()
            .unwrap();

        {
            let mut turn = session.turn();
            turn.create("The deployment region is us-west-2.")
                .await
                .unwrap();
            turn.compact().await.unwrap();
        }
        assert_eq!(session.history_len(), 2);
        assert!(session.history().any(crate::ResponseItem::is_user_message));
        assert!(
            session
                .history()
                .any(|item| matches!(item, crate::ResponseItem::Compaction { .. }))
        );

        session
            .turn()
            .create("Recall the deployment region.")
            .await
            .unwrap();

        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 3);
        assert_eq!(
            observations[1].previous_response_id.as_deref(),
            Some("resp-1")
        );
        assert!(observations[2].full_replay);
        assert_eq!(observations[2].previous_response_id, None);
        assert_eq!(observations[2].input.len(), 5);
    }

    #[derive(Clone)]
    struct FailingScripted;

    impl Service<crate::ResponsesAttempt> for FailingScripted {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(
            &mut self,
            _context: &mut std::task::Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: crate::ResponsesAttempt) -> Self::Future {
            Box::pin(async move {
                request
                    .emit(ResponseEvent::OutputTextDelta("uncommitted".to_owned()))
                    .await;
                Err(ResponseError::service(std::io::Error::other(
                    "scripted failure",
                )))
            })
        }
    }

    #[tokio::test]
    async fn failed_partial_response_never_commits_input_or_output() {
        let openai = OpenAi::builder("test-key")
            .service(|| FailingScripted)
            .build()
            .unwrap();
        let mut session = openai
            .instructions("Commit only complete Responses operations.")
            .build()
            .unwrap();
        {
            let mut turn = session.turn();
            let mut response = turn.create("This input must remain uncommitted.");

            assert!(matches!(
                response.try_next().await.unwrap(),
                Some(ResponseEvent::OutputTextDelta(delta)) if delta == "uncommitted"
            ));
            let error = response.try_next().await.unwrap_err();
            assert_eq!(error.to_string(), "scripted failure");
            assert!(response.await.is_err());
        }

        assert_eq!(session.history_len(), 0);
    }

    #[test]
    fn boxed_tower_errors_preserve_context_window_classification() {
        let service_error =
            crate::ResponsesServiceError::from(crate::ResponsesError::ContextWindowExceeded {
                event: r#"{"error":{"code":"context_length_exceeded"}}"#.to_owned(),
            });
        let error = ResponseError::from(Box::new(service_error) as tower::BoxError);

        assert!(error.is_context_window_exceeded());
        assert!(error.source().is_some());
    }

    #[tokio::test]
    async fn dropping_an_unpolled_response_performs_no_work() {
        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);
        let openai = OpenAi::builder("test-key")
            .service(move || Scripted {
                calls: Arc::clone(&factory_calls),
            })
            .build()
            .unwrap();
        let mut session = openai
            .instructions("Do not run abandoned operations.")
            .build()
            .unwrap();
        {
            let mut turn = session.turn();
            drop(turn.create("abandoned"));
        }

        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(session.history_len(), 0);
    }

    #[test]
    fn session_ids_are_serializable_uuid_v7_values() {
        let id = SessionId::new();
        assert_eq!(id.as_uuid().get_version_num(), 7);

        let encoded = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<SessionId>(&encoded).unwrap(), id);
        assert!(
            "550e8400-e29b-41d4-a716-446655440000"
                .parse::<SessionId>()
                .is_err()
        );
    }
}
