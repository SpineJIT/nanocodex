use std::{
    any::Any,
    collections::HashMap,
    panic::AssertUnwindSafe,
    path::Path,
    sync::{Arc, Mutex},
};

use futures_util::{FutureExt, StreamExt, stream::FuturesOrdered};
use nanocodex_oai_api::{
    __private::{EventSink, ModelConfig, ResponsesAttemptFactory},
    CONTEXT_WINDOW_TOKENS, MODEL, Prompt, Thinking,
    events::AgentEventKind,
    pricing::{ServiceTier, estimate},
    responses::{ContentItem, MessageRole, RequestProfile, ResponseItem, ToolDefinition, Usage},
    session::ManagedSessionState,
    tower::{
        CodeCall, CodeCallKind, GenerationOutput as TurnResult, ResponsesAttempt, ResponsesClient,
        ResponsesOutput, ResponsesServiceResponse,
    },
    transport::{ResponsesError, ResponsesTransport, TransportStats},
};
use nanocodex_oai_api::{compaction, context::assign_missing_response_item_id};
use serde::Serialize;
use serde_json::{Value, value::RawValue};
use tokio::sync::{RwLock, watch};
use tower::Service;
use tracing::{Instrument, info, info_span};
use web_time::Instant;

use super::{
    CompactionCompleted, CompactionFailed, CompactionStarted, ModelCallCompleted, ModelCallFailed,
    ModelCallStarted, RunError, RunStarted, RunStats, RunSteered, ToolCallArguments, ToolCallEvent,
    ToolResultEvent, WarmupCompleted, WarmupFailed, WarmupStarted,
    context::{ContextBaseline, ContextSnapshot, ContextState},
    display_endpoint, elapsed_ns,
    input::{
        custom_tool_notification, custom_tool_output, developer_context, function_tool_output,
        task_input, tool_search_output, turn_aborted,
    },
    terminal_payload,
};
use crate::{
    NanocodexError, Result,
    agent::{AgentSend, ContextSource},
    prompt_cache::ModelPromptCache,
    usage::TurnUsage,
};
use nanocodex_tools::{
    ToolContext, Tools,
    code_mode::{CodeModeExecution, CodeModeObserver, CodeModeUpdate},
    contract::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolInput, ToolOutput, ToolOutputBody},
    image::{prepare_output_images, prepare_user_input},
    runtime::{
        ImageGenerationConfig, OwnedToolContext, ToolRuntime, ToolRuntimeControl, WebSearchConfig,
    },
};

pub(crate) struct ModelRun<S> {
    events: EventSink,
    config: Arc<ModelConfig>,
    thinking: Thinking,
    fast_mode: bool,
    client: ResponsesClient<S>,
    transport_stats: Arc<TransportStats>,
    started_at: Instant,
    stats: RunStats,
    session: Option<ModelSessionState>,
    active_tools: Option<ToolRuntimeControl>,
    active_tool_calls: Vec<ActiveToolCall>,
    active_tool_batch_started_at: Option<Instant>,
    tool_call_indices: HashMap<Box<str>, u32>,
    tools: Tools,
    prompt_cache: ModelPromptCache,
    context_source: ContextSource,
    global_instructions: Option<Arc<str>>,
    force_compaction: bool,
}

pub(crate) enum ModelTurnOutcome {
    Completed(CompletedModelTurn),
    Cancelled(ModelCheckpoint),
    Failed {
        error: NanocodexError,
        checkpoint: ModelCheckpoint,
    },
}

pub(crate) enum ModelCompactOutcome {
    Completed(ModelCheckpoint),
    Cancelled(ModelCheckpoint),
    Failed {
        error: NanocodexError,
        checkpoint: ModelCheckpoint,
    },
}

pub(crate) struct CompletedModelTurn {
    pub(crate) final_message: String,
    pub(crate) usage: TurnUsage,
    pub(crate) checkpoint: ModelCheckpoint,
}

#[derive(Clone)]
pub(crate) struct ModelCheckpoint {
    workspace: String,
    conversation: ConversationState,
    request_prefix: Arc<[ResponseItem]>,
    prompt_cache_key: Arc<str>,
    preserve_inherited_delta: bool,
    global_instructions: Option<Arc<str>>,
    context_baseline: ContextBaseline,
}

pub(crate) struct PreparedCheckpoint {
    pub(crate) checkpoint: ModelCheckpoint,
    pub(crate) runtime: ToolRuntime,
    pub(crate) context_source: ContextSource,
    selected_agents_md: Option<Arc<str>>,
}

pub(crate) struct HistoryCheckpoint {
    pub(crate) workspace: String,
    pub(crate) canonical_context: ResponseItem,
    pub(crate) history: Vec<ResponseItem>,
    pub(crate) prompt_cache_key: Arc<str>,
    pub(crate) context_baseline: Option<ContextBaseline>,
}

impl ModelCheckpoint {
    pub(crate) fn workspace(&self) -> &str {
        &self.workspace
    }
    pub(crate) fn history(&self) -> nanocodex_oai_api::responses::ResponseHistory {
        self.conversation.shared_history()
    }

    #[allow(dead_code, reason = "consumed by the native durability boundary only")]
    pub(crate) const fn history_revision(&self) -> u64 {
        self.conversation.history_revision()
    }

    pub(crate) fn request_prefix(&self) -> &[ResponseItem] {
        &self.request_prefix
    }

    pub(crate) fn prompt_cache_key(&self) -> &str {
        &self.prompt_cache_key
    }

    pub(crate) fn canonical_context(&self) -> &ResponseItem {
        &self.conversation.canonical_context
    }

    pub(crate) fn snapshot_history(&self) -> Vec<ResponseItem> {
        self.conversation.flattened_history()
    }

    pub(crate) const fn context_baseline(&self) -> &ContextBaseline {
        &self.context_baseline
    }

    pub(crate) fn resume(
        workspace: String,
        mut request_prefix: Vec<ResponseItem>,
        prompt_cache_key: Arc<str>,
        canonical_context: ResponseItem,
        history: Vec<ResponseItem>,
        global_instructions: Option<Arc<str>>,
        context_baseline: Option<ContextBaseline>,
    ) -> Result<Self> {
        assign_request_prefix_ids(&mut request_prefix);
        let context_baseline =
            context_baseline.unwrap_or_else(|| ContextBaseline::reconstruct(&history));
        Ok(Self {
            workspace,
            conversation: ConversationState::resume(canonical_context, history)?,
            request_prefix: Arc::from(request_prefix),
            prompt_cache_key,
            preserve_inherited_delta: false,
            global_instructions,
            context_baseline,
        })
    }
}

struct ModelSessionState {
    workspace: String,
    tools: ToolRuntime,
    factory: ResponsesAttemptFactory,
    conversation: ConversationState,
    context: ContextState,
    preserve_inherited_delta: bool,
}

impl ModelSessionState {
    fn validate_workspace(&self, requested: Option<&str>) -> Result<()> {
        let Some(requested) = requested else {
            return Ok(());
        };
        if requested != self.workspace {
            return Err(NanocodexError::WorkspaceChanged {
                current: self.workspace.clone(),
                requested: requested.to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ActiveToolCall {
    call_id: String,
    name: String,
    kind: CodeCallKind,
    started_at: Instant,
    shell_abort_format: bool,
    completion: Arc<Mutex<Option<CompletedToolCall>>>,
    progress: Arc<Mutex<ActiveToolProgress>>,
    execution_started_at: Arc<Mutex<Option<Instant>>>,
    span: tracing::Span,
}

#[derive(Default)]
struct ActiveToolProgress {
    nested_tool_calls: u32,
}

struct CompletedToolCall {
    call_id: String,
    tool: String,
    success: bool,
    duration_ns: u64,
    work_duration_ns: u64,
    output: ToolOutputBody,
    metadata: Option<Box<RawValue>>,
    response_items: Vec<ResponseItem>,
}

struct NestedToolEventObserver<'a> {
    events: &'a EventSink,
    tool_call_indices: &'a HashMap<Box<str>, u32>,
    progress: &'a Mutex<ActiveToolProgress>,
    fallback_call_index: u32,
    parent_call_id: &'a str,
    error: Option<NanocodexError>,
}

impl CodeModeObserver for NestedToolEventObserver<'_> {
    fn update(&mut self, update: CodeModeUpdate<'_>) {
        if self.error.is_some() {
            return;
        }
        let result = match update {
            CodeModeUpdate::NestedCallStarted {
                call_id,
                name,
                input,
            } => {
                let (call_id, call_index) = self.event_context(call_id);
                let result = self.events.emit(
                    AgentEventKind::ToolCall,
                    ToolCallEvent {
                        call_id: &call_id,
                        tool: name,
                        arguments: input,
                        model_call_index: call_index,
                    },
                );
                if result.is_ok() {
                    self.progress
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .nested_tool_calls += 1;
                }
                result
            }
            CodeModeUpdate::NestedCallCompleted(call) => {
                let (call_id, _) = self.event_context(&call.call_id);
                self.events.emit(
                    AgentEventKind::ToolResult,
                    ToolResultEvent {
                        call_id: &call_id,
                        tool: &call.name,
                        status: status(call.success),
                        duration_ns: call.duration_ns,
                        started_after_ns: Some(call.started_after_ns),
                        result: &call.output,
                        metadata: call.metadata.as_deref(),
                    },
                )
            }
        };
        if let Err(error) = result {
            self.error = Some(error.into());
        }
    }
}

impl NestedToolEventObserver<'_> {
    fn event_context(&self, nested_call_id: &str) -> (String, u32) {
        let embedded_parent = nested_call_id
            .rsplit_once("/code-")
            .map(|(parent, _)| parent);
        let original_parent = embedded_parent.unwrap_or(self.parent_call_id);
        let call_id = embedded_parent.map_or_else(
            || format!("{}/{nested_call_id}", self.parent_call_id),
            |_| nested_call_id.to_owned(),
        );
        let call_index = self
            .tool_call_indices
            .get(original_parent)
            .copied()
            .unwrap_or(self.fallback_call_index);
        (call_id, call_index)
    }
}

async fn execute_code_call(
    tools: &ToolRuntime,
    call: &CodeCall,
    owned_context: Option<OwnedToolContext>,
    session_id: &str,
    observer: &mut dyn CodeModeObserver,
    tool_span: &tracing::Span,
) -> CodeModeExecution {
    if let Some(context) = owned_context {
        tools
            .execute_code_owned_with_updates(&call.input, context, observer)
            .instrument(tool_span.clone())
            .await
    } else {
        let context = ToolContext::new(
            MODEL,
            session_id,
            &call.call_id,
            &[],
            DEFAULT_TOOL_OUTPUT_TOKENS,
        );
        tools
            .wait_for_code_with_updates(&call.input, context, observer)
            .instrument(tool_span.clone())
            .await
    }
}

struct WarmupExecution {
    response_id: String,
    attempt: u32,
    connection_generation: u32,
    usage: Option<Usage>,
    server_reasoning_included: bool,
}

struct WarmupOutcome {
    response_id: Option<String>,
    server_reasoning_included: bool,
}

enum ModelTaskOutcome {
    Completed(String),
    Cancelled,
}

#[derive(Clone, Copy)]
enum CompactionPhase {
    PreTurn,
    MidTurn,
}

struct CompactionContext<'a> {
    snapshot: Option<&'a ContextSnapshot>,
    phase: CompactionPhase,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ContinuationPolicy {
    thinking: Thinking,
    fast_mode: bool,
}

#[derive(Clone)]
struct ConversationState {
    canonical_context: Arc<ResponseItem>,
    managed: ManagedSessionState,
    continuation_policy: Option<ContinuationPolicy>,
}

impl ConversationState {
    fn empty(canonical_context: ResponseItem) -> Self {
        Self {
            canonical_context: Arc::new(canonical_context),
            managed: ManagedSessionState::new(Vec::new()),
            continuation_policy: None,
        }
    }

    fn new(history: Vec<ResponseItem>) -> Result<Self> {
        let canonical_context = history
            .iter()
            .find(|item| item.is_user_message())
            .cloned()
            .ok_or(NanocodexError::MalformedResponse {
                detail: "task input did not include initial context",
            })?;
        Ok(Self {
            canonical_context: Arc::new(canonical_context),
            managed: ManagedSessionState::new(history),
            continuation_policy: None,
        })
    }

    fn resume(mut canonical_context: ResponseItem, history: Vec<ResponseItem>) -> Result<Self> {
        if !canonical_context.is_user_message() {
            return Err(NanocodexError::InvalidSessionSnapshot(
                "canonical context must be a user message".to_owned(),
            ));
        }
        assign_missing_response_item_id(&mut canonical_context);
        let managed = ManagedSessionState::resume(history)
            .map_err(|error| NanocodexError::InvalidSessionSnapshot(error.to_string()))?;
        Ok(Self {
            canonical_context: Arc::new(canonical_context),
            managed,
            continuation_policy: None,
        })
    }

    fn flattened_history(&self) -> Vec<ResponseItem> {
        self.managed.flattened_history()
    }

    fn clear_delta(&mut self) {
        self.managed.clear_delta();
    }

    fn append(&mut self, items: impl IntoIterator<Item = ResponseItem>) {
        self.managed.append(items);
    }

    fn update_token_info(&mut self, usage: Option<&Usage>) {
        self.managed.update_token_info(usage);
    }

    const fn observe_server_reasoning(&mut self, included: bool) {
        self.managed.observe_server_reasoning(included);
    }

    fn active_context_tokens(&self) -> u64 {
        self.managed.active_context_tokens()
    }

    fn prompt_history(&self) -> nanocodex_oai_api::responses::ResponseHistory {
        self.managed.prompt_history()
    }

    fn prompt_history_with_repair(&self) -> (nanocodex_oai_api::responses::ResponseHistory, bool) {
        self.managed.prompt_history_with_repair()
    }

    fn adopt_prompt_history(&mut self, history: nanocodex_oai_api::responses::ResponseHistory) {
        self.managed.adopt_prompt_history(history);
    }

    fn shared_history(&self) -> nanocodex_oai_api::responses::ResponseHistory {
        self.managed.shared_history()
    }

    const fn delta_start(&self) -> usize {
        self.managed.delta_start()
    }

    fn previous_response_id(&self) -> Option<&str> {
        self.managed.previous_response_id()
    }

    fn set_previous_response_id(&mut self, response_id: impl Into<String>) {
        self.managed.set_previous_response_id(response_id);
    }

    #[allow(dead_code, reason = "consumed by the native durability boundary only")]
    const fn history_revision(&self) -> u64 {
        self.managed.history_revision()
    }

    fn install_pre_turn_compaction(&mut self, item: ResponseItem, request_prefix: &[ResponseItem]) {
        self.managed.install_compaction(item, [], request_prefix);
    }

    fn install_mid_turn_compaction(
        &mut self,
        item: ResponseItem,
        canonical_developer_context: ResponseItem,
        canonical_context: ResponseItem,
        request_prefix: &[ResponseItem],
    ) {
        self.canonical_context = Arc::new(canonical_context.clone());
        let initial_context = [canonical_developer_context, canonical_context];
        self.managed
            .install_compaction(item, initial_context, request_prefix);
    }

    fn append_canonical_context(
        &mut self,
        canonical_developer_context: ResponseItem,
        canonical_context: ResponseItem,
    ) {
        self.canonical_context = Arc::new(canonical_context.clone());
        self.managed
            .append([canonical_developer_context, canonical_context]);
    }

    fn set_canonical_context(&mut self, canonical_context: ResponseItem) {
        self.canonical_context = Arc::new(canonical_context);
    }

    fn reset_for_full_request(&mut self) {
        self.managed.reset_for_full_request();
    }

    fn prepare_request_policy(&mut self, policy: ContinuationPolicy) {
        if self
            .continuation_policy
            .is_some_and(|previous| previous != policy)
        {
            self.reset_for_full_request();
        }
        self.continuation_policy = Some(policy);
    }

    fn commit(&mut self) -> Result<()> {
        self.managed
            .commit()
            .map_err(|_| NanocodexError::MalformedResponse {
                detail: "completed turn did not have a response ID",
            })
    }

    fn commit_interrupted(&mut self) {
        self.managed.commit_interrupted();
    }

    fn commit_tail(&mut self) {
        self.managed.commit_tail();
    }
}

impl<S> ModelRun<S> {
    pub(crate) fn new(
        events: EventSink,
        config: Arc<ModelConfig>,
        client: ResponsesClient<S>,
        transport_stats: Arc<TransportStats>,
        tools: Tools,
        prompt_cache: ModelPromptCache,
        context_source: ContextSource,
    ) -> Self {
        let thinking = config.thinking;
        let fast_mode = config.fast_mode;
        let global_instructions = context_source.global_instructions();
        Self {
            events,
            config,
            thinking,
            fast_mode,
            client,
            transport_stats,
            started_at: Instant::now(),
            stats: RunStats::default(),
            session: None,
            active_tools: None,
            active_tool_calls: Vec::new(),
            active_tool_batch_started_at: None,
            tool_call_indices: HashMap::new(),
            tools,
            prompt_cache,
            context_source,
            global_instructions,
            force_compaction: false,
        }
    }

    pub(crate) fn from_checkpoint(
        events: EventSink,
        config: Arc<ModelConfig>,
        client: ResponsesClient<S>,
        transport_stats: Arc<TransportStats>,
        tools: Tools,
        prompt_cache: ModelPromptCache,
        prepared: PreparedCheckpoint,
    ) -> Self {
        let PreparedCheckpoint {
            checkpoint,
            runtime,
            context_source,
            selected_agents_md,
        } = prepared;
        let active_tools = runtime.control();
        let factory = ResponsesAttemptFactory::new(
            RequestProfile::new(
                events.request_id(),
                checkpoint.prompt_cache_key.to_string(),
                Arc::clone(&checkpoint.request_prefix),
            ),
            events.clone(),
            Arc::clone(&transport_stats),
        );
        let thinking = config.thinking;
        let fast_mode = config.fast_mode;
        let context_source =
            context_source.with_fallback_global(checkpoint.global_instructions.clone());
        let global_instructions = context_source.global_instructions();
        Self {
            events,
            config,
            thinking,
            fast_mode,
            client,
            transport_stats,
            started_at: Instant::now(),
            stats: RunStats::default(),
            session: Some(ModelSessionState {
                workspace: checkpoint.workspace,
                tools: runtime,
                factory,
                conversation: checkpoint.conversation,
                context: ContextState::new(selected_agents_md, checkpoint.context_baseline),
                preserve_inherited_delta: checkpoint.preserve_inherited_delta,
            }),
            active_tools: Some(active_tools),
            active_tool_calls: Vec::new(),
            active_tool_batch_started_at: None,
            tool_call_indices: HashMap::new(),
            tools,
            prompt_cache,
            context_source,
            global_instructions,
            force_compaction: false,
        }
    }

    pub(crate) fn set_events(&mut self, events: EventSink) {
        if let Some(session) = &mut self.session {
            session.factory.set_events(events.clone());
        }
        self.events = events;
    }

    pub(crate) fn replace_client(&mut self, client: ResponsesClient<S>) {
        self.client = client;
    }

    pub(crate) async fn shutdown(&mut self) {
        if let Some(tools) = &self.active_tools {
            tools.cancel().await;
        }
    }

    fn empty_session(&mut self, requested_workspace: Option<&str>) -> Result<ModelSessionState> {
        let workspace = requested_workspace.map_or_else(
            || self.context_source.resolve_workspace(None),
            |workspace| Ok(workspace.to_owned()),
        )?;
        let selected_agents_md = self
            .context_source
            .project_instructions(&workspace)
            .map(Arc::<str>::from);
        let tools = tool_runtime(&workspace, &self.config, &self.tools);
        let tool_control = tools.control();
        self.active_tools = Some(tool_control);
        let factory = self.attempt_factory(&tools);
        let context = ContextState::new(selected_agents_md, ContextBaseline::Missing);
        let canonical_context = context
            .capture(tools.working_directory(), tools.default_shell_name())
            .full_item();
        Ok(ModelSessionState {
            workspace,
            tools,
            factory,
            conversation: ConversationState::empty(canonical_context),
            context,
            preserve_inherited_delta: false,
        })
    }

    fn attempt_factory(&self, tools: &ToolRuntime) -> ResponsesAttemptFactory {
        attempt_factory(
            &self.events,
            &self.transport_stats,
            self.prompt_cache.key(),
            tools,
            self.config.system_prompt(),
        )
    }

    fn responses_endpoint(&self) -> &str {
        match self.config.responses_transport {
            ResponsesTransport::WebSocket => &self.config.websocket_url,
            ResponsesTransport::Https => &self.config.api_base_url,
        }
    }
}

pub(crate) fn prepare_checkpoint(
    checkpoint: ModelCheckpoint,
    config: &ModelConfig,
    tools: &Tools,
    context_source: ContextSource,
) -> PreparedCheckpoint {
    let runtime = tool_runtime(checkpoint.workspace(), config, tools);
    let selected_agents_md = context_source
        .project_instructions(checkpoint.workspace())
        .map(Arc::from);
    PreparedCheckpoint {
        checkpoint,
        runtime,
        context_source,
        selected_agents_md,
    }
}

pub(crate) fn prepare_resumed_checkpoint(
    mut checkpoint: ModelCheckpoint,
    config: &ModelConfig,
    tools: &Tools,
    session_id: &str,
    context_source: ContextSource,
) -> Result<PreparedCheckpoint> {
    checkpoint.global_instructions = context_source
        .global_instructions()
        .or(checkpoint.global_instructions);
    let prepared = prepare_checkpoint(checkpoint, config, tools, context_source);
    let tool_specs = prepared.runtime.model_specs(session_id);
    let expected = request_profile(
        "resume-validation",
        "resume-validation",
        tool_specs,
        config.system_prompt(),
    );
    let expected =
        serde_json::to_vec(&without_response_item_ids(expected.prefix())).map_err(|error| {
            NanocodexError::InvalidSessionSnapshot(format!(
                "failed to validate the request prefix: {error}"
            ))
        })?;
    let stored = serde_json::to_vec(&without_response_item_ids(
        prepared.checkpoint.request_prefix(),
    ))
    .map_err(|error| {
        NanocodexError::InvalidSessionSnapshot(format!(
            "failed to validate the stored request prefix: {error}"
        ))
    })?;
    if expected != stored {
        return Err(NanocodexError::InvalidSessionSnapshot(
            "instructions or tool definitions do not match the resumed session".to_owned(),
        ));
    }
    Ok(prepared)
}

pub(crate) fn prepare_history_checkpoint(
    resume: HistoryCheckpoint,
    config: &ModelConfig,
    tools: &Tools,
    session_id: &str,
    context_source: ContextSource,
) -> Result<PreparedCheckpoint> {
    let HistoryCheckpoint {
        workspace,
        canonical_context,
        history,
        prompt_cache_key,
        context_baseline,
    } = resume;
    let selected_agents_md = context_source
        .project_instructions(&workspace)
        .map(Arc::from);
    let runtime = tool_runtime(&workspace, config, tools);
    let tool_specs = runtime.model_specs(session_id);
    let request_prefix = request_profile(
        "history-resume",
        "history-resume",
        tool_specs,
        config.system_prompt(),
    )
    .prefix()
    .to_vec();
    let checkpoint = ModelCheckpoint::resume(
        workspace,
        request_prefix,
        prompt_cache_key,
        canonical_context,
        history,
        context_source.global_instructions(),
        context_baseline,
    )?;
    Ok(PreparedCheckpoint {
        checkpoint,
        runtime,
        context_source,
        selected_agents_md,
    })
}

fn without_response_item_ids(items: &[ResponseItem]) -> Vec<ResponseItem> {
    items
        .iter()
        .cloned()
        .map(|mut item| {
            item.strip_id();
            item
        })
        .collect()
}

impl<S> ModelRun<S>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + AgentSend + 'static,
    S::Error: Into<NanocodexError>,
    S::Future: AgentSend,
{
    pub(crate) async fn compact(
        &mut self,
        requested_workspace: Option<Arc<str>>,
        thinking: Thinking,
        fast_mode: bool,
        logical_turn: u64,
        cancel: &mut tokio::sync::oneshot::Receiver<()>,
    ) -> Result<ModelCompactOutcome> {
        self.thinking = thinking;
        self.fast_mode = fast_mode;
        self.started_at = Instant::now();
        self.stats = RunStats::default();
        let mut session = match self.session.take() {
            Some(session) => session,
            None => self.empty_session(requested_workspace.as_deref())?,
        };
        session.factory = session.factory.for_logical_turn(logical_turn);
        if let Err(error) = session.validate_workspace(requested_workspace.as_deref()) {
            let checkpoint =
                Self::checkpoint_from_session(&session, false, self.global_instructions.clone());
            self.session = Some(session);
            return Ok(ModelCompactOutcome::Failed { error, checkpoint });
        }
        session
            .conversation
            .prepare_request_policy(self.continuation_policy());

        let active_context_tokens = session.conversation.active_context_tokens();
        let previous_response_id = session
            .conversation
            .previous_response_id()
            .map(str::to_owned);
        let auto_compact_token_limit =
            compaction::auto_compact_token_limit(MODEL).unwrap_or(CONTEXT_WINDOW_TOKENS);
        let compacted = {
            let compaction = self.perform_compaction(
                self.stats.model_calls,
                session.conversation.prompt_history(),
                session.conversation.delta_start(),
                previous_response_id.as_deref(),
                active_context_tokens,
                auto_compact_token_limit,
                &session.factory,
            );
            tokio::pin!(compaction);
            tokio::select! {
                biased;
                _ = &mut *cancel => None,
                outcome = &mut compaction => Some(outcome),
            }
        };
        let Some(compacted) = compacted else {
            session.conversation.reset_for_full_request();
            let checkpoint =
                Self::checkpoint_from_session(&session, false, self.global_instructions.clone());
            self.session = Some(session);
            return Ok(ModelCompactOutcome::Cancelled(checkpoint));
        };
        let (item, _usage, server_reasoning_included) = match compacted {
            Ok(compacted) => compacted,
            Err(error) => {
                session.conversation.reset_for_full_request();
                let checkpoint = Self::checkpoint_from_session(
                    &session,
                    false,
                    self.global_instructions.clone(),
                );
                self.session = Some(session);
                return Ok(ModelCompactOutcome::Failed { error, checkpoint });
            }
        };
        session
            .conversation
            .observe_server_reasoning(server_reasoning_included);
        session
            .conversation
            .install_pre_turn_compaction(item, session.factory.profile().prefix());
        session.conversation.commit_tail();
        session.context.require_full_reinjection();
        session.preserve_inherited_delta = false;
        self.force_compaction = false;
        let checkpoint =
            Self::checkpoint_from_session(&session, false, self.global_instructions.clone());
        self.session = Some(session);
        Ok(ModelCompactOutcome::Completed(checkpoint))
    }

    pub(crate) fn emit_cancelled_before_start(
        &mut self,
        task: &Prompt,
        workspace: Option<&str>,
        thinking: Thinking,
        fast_mode: bool,
    ) -> Result<()> {
        self.thinking = thinking;
        self.fast_mode = fast_mode;
        self.started_at = Instant::now();
        self.stats = RunStats::default();
        self.events.emit(
            AgentEventKind::RunStarted,
            RunStarted {
                mode: "openai_model",
                model: MODEL,
                reasoning_mode: self.config.reasoning_mode.as_str(),
                effort: self.thinking.as_str(),
                transport: self.config.responses_transport.as_str(),
                orchestration: ModelConfig::orchestration(),
                websocket_url: display_endpoint(self.responses_endpoint()),
                workspace,
                instruction_bytes: task.instruction.text_bytes(),
            },
        )?;
        let error = NanocodexError::TurnCancelled;
        let message = error.to_string();
        self.events
            .emit(AgentEventKind::RunError, RunError { message: &message })?;
        let usage = self.stats.turn_usage(self.fast_mode);
        record_turn_usage(&tracing::Span::current(), &usage);
        self.events.emit(
            AgentEventKind::RunFailed,
            terminal_payload(
                "cancelled",
                self.started_at.elapsed(),
                &self.config,
                self.thinking,
                &self.stats,
                &usage,
            ),
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute(
        &mut self,
        task: Prompt,
        workspace: Option<Arc<str>>,
        thinking: Thinking,
        fast_mode: bool,
        logical_turn: u64,
        steers: tokio::sync::mpsc::Receiver<Prompt>,
        mut cancel: tokio::sync::oneshot::Receiver<()>,
        fork_snapshots: watch::Sender<Option<ModelCheckpoint>>,
    ) -> Result<ModelTurnOutcome> {
        self.thinking = thinking;
        self.fast_mode = fast_mode;
        self.started_at = Instant::now();
        self.stats = RunStats::default();
        if let Some(tools) = &self.active_tools {
            tools.begin_turn();
        }
        let transport_before = self.transport_stats.snapshot();
        self.events.emit(
            AgentEventKind::RunStarted,
            RunStarted {
                mode: "openai_model",
                model: MODEL,
                reasoning_mode: self.config.reasoning_mode.as_str(),
                effort: self.thinking.as_str(),
                transport: self.config.responses_transport.as_str(),
                orchestration: ModelConfig::orchestration(),
                websocket_url: display_endpoint(self.responses_endpoint()),
                workspace: workspace.as_deref(),
                instruction_bytes: task.instruction.text_bytes(),
            },
        )?;

        let outcome = self
            .execute_task(
                task,
                workspace,
                logical_turn,
                steers,
                &mut cancel,
                &fork_snapshots,
            )
            .await;
        let elapsed = self.started_at.elapsed();
        match outcome {
            Ok(ModelTaskOutcome::Completed(message)) => {
                self.stats
                    .apply_transport(self.transport_stats.since(transport_before));
                let usage = self.stats.turn_usage(self.fast_mode);
                record_turn_usage(&tracing::Span::current(), &usage);
                self.events.emit(
                    AgentEventKind::RunCompleted,
                    terminal_payload(
                        "completed",
                        elapsed,
                        &self.config,
                        self.thinking,
                        &self.stats,
                        &usage,
                    ),
                )?;
                let checkpoint = self.commit_checkpoint()?;
                Ok(ModelTurnOutcome::Completed(CompletedModelTurn {
                    final_message: message,
                    usage,
                    checkpoint,
                }))
            }
            Ok(ModelTaskOutcome::Cancelled) => {
                if let Some(tools) = &self.active_tools {
                    tools.cancel_turn().await;
                }
                let checkpoint = self.commit_interrupted_checkpoint()?;
                let elapsed = self.started_at.elapsed();
                let error = NanocodexError::TurnCancelled;
                let message = error.to_string();
                self.events
                    .emit(AgentEventKind::RunError, RunError { message: &message })?;
                self.stats
                    .apply_transport(self.transport_stats.since(transport_before));
                let usage = self.stats.turn_usage(self.fast_mode);
                record_turn_usage(&tracing::Span::current(), &usage);
                self.events.emit(
                    AgentEventKind::RunFailed,
                    terminal_payload(
                        "cancelled",
                        elapsed,
                        &self.config,
                        self.thinking,
                        &self.stats,
                        &usage,
                    ),
                )?;
                Ok(ModelTurnOutcome::Cancelled(checkpoint))
            }
            Err(error) => {
                if error
                    .responses_error()
                    .is_some_and(ResponsesError::is_context_window_exceeded)
                {
                    // The provider is authoritative about the usable context
                    // window. Compact before the next model attempt even when
                    // local usage remains below the proactive threshold.
                    self.force_compaction = true;
                }
                let checkpoint = if self.active_tool_calls.is_empty() {
                    self.finish_active_tool_batch_wall();
                    // Retain client-authored state at its safe boundary, but
                    // drop the transport checkpoint: the provider may have
                    // observed the failed request without returning a usable
                    // continuation.
                    if let Some(session) = &mut self.session {
                        session.conversation.commit_interrupted();
                        session.preserve_inherited_delta = false;
                    }
                    self.session.as_ref().map(|session| {
                        Self::checkpoint_from_session(
                            session,
                            false,
                            self.global_instructions.clone(),
                        )
                    })
                } else {
                    // A tool-dispatch failure may leave sibling calls in
                    // flight. Stop their retained runtime work, preserve every
                    // completed slot, and synthesize outputs for only the
                    // unfinished calls before committing the failure boundary.
                    if let Some(tools) = &self.active_tools {
                        tools.cancel_turn().await;
                    }
                    Some(self.commit_interrupted_checkpoint()?)
                };
                let message = error.to_string();
                self.events
                    .emit(AgentEventKind::RunError, RunError { message: &message })?;
                self.stats
                    .apply_transport(self.transport_stats.since(transport_before));
                let usage = self.stats.turn_usage(self.fast_mode);
                record_turn_usage(&tracing::Span::current(), &usage);
                self.events.emit(
                    AgentEventKind::RunFailed,
                    terminal_payload(
                        "failed",
                        elapsed,
                        &self.config,
                        self.thinking,
                        &self.stats,
                        &usage,
                    ),
                )?;
                match checkpoint {
                    Some(checkpoint) => Ok(ModelTurnOutcome::Failed { error, checkpoint }),
                    None => Err(error),
                }
            }
        }
    }

    async fn prepare_follow_on_turn(
        &mut self,
        session: &mut ModelSessionState,
        task: &Prompt,
        cancel: &mut tokio::sync::oneshot::Receiver<()>,
    ) -> Result<bool> {
        let compacted = {
            let compaction = self.maybe_compact(
                self.stats.model_calls,
                &mut session.conversation,
                &session.factory,
                CompactionContext {
                    snapshot: session.context.snapshot(),
                    phase: CompactionPhase::PreTurn,
                },
            );
            tokio::pin!(compaction);
            tokio::select! {
                biased;
                _ = &mut *cancel => None,
                outcome = &mut compaction => Some(outcome?),
            }
        };
        let Some(compacted) = compacted else {
            let user_content = prepare_user_input(&task.instruction).await;
            session
                .conversation
                .append([ResponseItem::message(MessageRole::User, user_content)]);
            return Ok(false);
        };
        if compacted || session.preserve_inherited_delta {
            session.preserve_inherited_delta = false;
        } else {
            session.conversation.clear_delta();
        }
        if compacted {
            session.context.require_full_reinjection();
        }
        let current_context = session.context.capture(
            session.tools.working_directory(),
            session.tools.default_shell_name(),
        );
        let canonical_context = current_context.full_item();
        if let Some(update) = session.context.update(current_context) {
            if update.full {
                session
                    .conversation
                    .append_canonical_context(developer_context(), update.item);
            } else {
                session.conversation.append([update.item]);
                session
                    .conversation
                    .set_canonical_context(canonical_context);
            }
        } else {
            session
                .conversation
                .set_canonical_context(canonical_context);
        }
        let user_content = prepare_user_input(&task.instruction).await;
        session
            .conversation
            .append([ResponseItem::message(MessageRole::User, user_content)]);
        Ok(true)
    }

    async fn execute_task(
        &mut self,
        task: Prompt,
        requested_workspace: Option<Arc<str>>,
        logical_turn: u64,
        steers: tokio::sync::mpsc::Receiver<Prompt>,
        cancel: &mut tokio::sync::oneshot::Receiver<()>,
        fork_snapshots: &watch::Sender<Option<ModelCheckpoint>>,
    ) -> Result<ModelTaskOutcome> {
        let mut session = if let Some(mut session) = self.session.take() {
            session.factory = session.factory.for_logical_turn(logical_turn);
            if let Err(error) = session.validate_workspace(requested_workspace.as_deref()) {
                self.session = Some(session);
                return Err(error);
            }
            session
                .conversation
                .prepare_request_policy(self.continuation_policy());
            match self
                .prepare_follow_on_turn(&mut session, &task, cancel)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    self.session = Some(session);
                    return Ok(ModelTaskOutcome::Cancelled);
                }
                Err(error) => {
                    self.session = Some(session);
                    return Err(error);
                }
            }
            session
        } else {
            // The owning driver resolves its workspace before accepting
            // prompts. Reuse that stable path instead of introducing a second
            // filesystem failure between acceptance and session creation.
            let workspace = requested_workspace.map_or_else(
                || self.context_source.resolve_workspace(None),
                |workspace| Ok(workspace.to_string()),
            )?;
            let selected_agents_md = self
                .context_source
                .project_instructions(&workspace)
                .map(Arc::<str>::from);
            let tools = tool_runtime(&workspace, &self.config, &self.tools);
            let tool_control = tools.control();
            tool_control.begin_turn();
            self.active_tools = Some(tool_control);
            let factory = self.attempt_factory(&tools).for_logical_turn(logical_turn);
            let user_content = prepare_user_input(&task.instruction).await;
            let mut context = ContextState::new(selected_agents_md, ContextBaseline::Missing);
            let context_snapshot =
                context.capture(tools.working_directory(), tools.default_shell_name());
            let history = task_input(user_content, &context_snapshot);
            context.establish(context_snapshot);
            let conversation = ConversationState::new(history)?;
            let mut session = ModelSessionState {
                workspace,
                tools,
                factory,
                conversation,
                context,
                preserve_inherited_delta: false,
            };
            session
                .conversation
                .prepare_request_policy(self.continuation_policy());
            Self::publish_fork_snapshot(
                &mut session,
                fork_snapshots,
                self.global_instructions.as_ref(),
            );
            let warmup = {
                let warmup = self.perform_warmup(&session.factory);
                tokio::pin!(warmup);
                tokio::select! {
                    biased;
                    _ = &mut *cancel => None,
                    outcome = &mut warmup => Some(outcome),
                }
            };
            let Some(warmup) = warmup else {
                self.session = Some(session);
                return Ok(ModelTaskOutcome::Cancelled);
            };
            match warmup {
                Ok(outcome) => {
                    session
                        .conversation
                        .observe_server_reasoning(outcome.server_reasoning_included);
                    if let Some(response_id) = outcome.response_id {
                        session.conversation.set_previous_response_id(response_id);
                    } else {
                        session.conversation.reset_for_full_request();
                        self.stats.last_response_id = None;
                    }
                }
                Err(error) if error.responses_error().is_some() => {
                    session.conversation.reset_for_full_request();
                    self.stats.last_response_id = None;
                }
                Err(error) => {
                    self.session = Some(session);
                    return Err(error);
                }
            }
            session
        };

        let outcome = {
            let task = self.drive_session(&mut session, steers, fork_snapshots);
            tokio::pin!(task);
            tokio::select! {
                biased;
                _ = &mut *cancel => None,
                outcome = &mut task => Some(outcome),
            }
        };
        self.session = Some(session);
        match outcome {
            Some(outcome) => outcome.map(ModelTaskOutcome::Completed),
            None => Ok(ModelTaskOutcome::Cancelled),
        }
    }

    const fn continuation_policy(&self) -> ContinuationPolicy {
        ContinuationPolicy {
            thinking: self.thinking,
            fast_mode: self.fast_mode,
        }
    }

    fn commit_checkpoint(&mut self) -> Result<ModelCheckpoint> {
        let session = self
            .session
            .as_mut()
            .ok_or(NanocodexError::InvalidAttemptState {
                detail: "completed turn did not have a model session",
            })?;
        session.conversation.commit()?;
        Ok(Self::checkpoint_from_session(
            session,
            false,
            self.global_instructions.clone(),
        ))
    }

    fn commit_interrupted_checkpoint(&mut self) -> Result<ModelCheckpoint> {
        let mut aborted_outputs = Vec::with_capacity(self.active_tool_calls.len());
        for call in std::mem::take(&mut self.active_tool_calls) {
            let completed = call
                .completion
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(completed) = completed {
                aborted_outputs.extend(self.finish_completed_tool_call(completed, &call.progress)?);
                continue;
            }
            let duration_ns = elapsed_ns(call.started_at);
            let elapsed_seconds = call.started_at.elapsed().as_secs_f64();
            let output = ToolOutputBody::Text(if call.shell_abort_format {
                format!("Wall time: {elapsed_seconds:.1} seconds\naborted by user")
            } else {
                format!("aborted by user after {:.1}s", elapsed_seconds.max(0.1))
            });
            self.finish_active_tool_progress(&call.progress);
            self.finish_cancelled_tool_work(&call);
            record_tool_span_terminal(&call.span, "cancelled", "ERROR", duration_ns, &output);
            self.events.emit(
                AgentEventKind::ToolResult,
                ToolResultEvent {
                    call_id: &call.call_id,
                    tool: &call.name,
                    status: "cancelled",
                    duration_ns,
                    started_after_ns: None,
                    result: &output,
                    metadata: None,
                },
            )?;
            aborted_outputs.push(match call.kind {
                CodeCallKind::Custom => custom_tool_output(call.call_id, output),
                CodeCallKind::Function => function_tool_output(call.call_id, output),
                CodeCallKind::ToolSearch => tool_search_output(call.call_id.clone(), Vec::new()),
            });
        }
        self.finish_active_tool_batch_wall();
        let session = self
            .session
            .as_mut()
            .ok_or(NanocodexError::InvalidAttemptState {
                detail: "interrupted turn did not have a model session",
            })?;
        session.conversation.append(aborted_outputs);
        session.conversation.append([turn_aborted()]);
        session.conversation.commit_interrupted();
        Ok(Self::checkpoint_from_session(
            session,
            false,
            self.global_instructions.clone(),
        ))
    }

    fn checkpoint_from_session(
        session: &ModelSessionState,
        preserve_inherited_delta: bool,
        global_instructions: Option<Arc<str>>,
    ) -> ModelCheckpoint {
        ModelCheckpoint {
            workspace: session.workspace.clone(),
            conversation: session.conversation.clone(),
            request_prefix: session.factory.profile().shared_prefix(),
            prompt_cache_key: Arc::from(session.factory.profile().prompt_cache_key()),
            preserve_inherited_delta,
            global_instructions,
            context_baseline: session.context.baseline(),
        }
    }

    fn publish_fork_snapshot(
        session: &mut ModelSessionState,
        snapshots: &watch::Sender<Option<ModelCheckpoint>>,
        global_instructions: Option<&Arc<str>>,
    ) {
        session.conversation.commit_tail();
        snapshots.send_replace(Some(ModelCheckpoint {
            workspace: session.workspace.clone(),
            conversation: session.conversation.clone(),
            request_prefix: session.factory.profile().shared_prefix(),
            prompt_cache_key: Arc::from(session.factory.profile().prompt_cache_key()),
            preserve_inherited_delta: true,
            global_instructions: global_instructions.cloned(),
            context_baseline: session.context.baseline(),
        }));
    }

    async fn drive_session(
        &mut self,
        session: &mut ModelSessionState,
        mut steers: tokio::sync::mpsc::Receiver<Prompt>,
        fork_snapshots: &watch::Sender<Option<ModelCheckpoint>>,
    ) -> Result<String> {
        // Match Codex's ordering: always sample the turn's initial prompt once
        // before injecting input that arrived while that first request ran.
        let mut can_drain_steers = false;
        loop {
            if can_drain_steers {
                self.drain_steers(&mut session.conversation, &mut steers)
                    .await?;
            }
            Self::publish_fork_snapshot(session, fork_snapshots, self.global_instructions.as_ref());
            let call_index = self.stats.model_calls + 1;
            let response = self
                .perform_model_call(call_index, &mut session.conversation, &session.factory)
                .await?;
            session
                .conversation
                .update_token_info(response.usage.as_ref());
            session
                .conversation
                .set_previous_response_id(response.id.clone());
            let end_turn = response.end_turn;
            let final_message = response.final_message;
            let code_calls = response.code_calls;
            session.conversation.append(response.output_items);
            can_drain_steers = true;

            if code_calls.is_empty() {
                if end_turn == Some(false) {
                    session.conversation.clear_delta();
                    let compacted = self
                        .maybe_compact(
                            call_index,
                            &mut session.conversation,
                            &session.factory,
                            CompactionContext {
                                snapshot: session.context.snapshot(),
                                phase: CompactionPhase::MidTurn,
                            },
                        )
                        .await?;
                    // After a mid-turn compaction, resume the model-requested
                    // continuation before injecting newer steering input.
                    can_drain_steers = !compacted;
                    continue;
                }
                if !steers.is_empty() {
                    // The completed response is retained by previous_response_id;
                    // the next delta contains only newly drained steer messages.
                    session.conversation.clear_delta();
                    self.maybe_compact(
                        call_index,
                        &mut session.conversation,
                        &session.factory,
                        CompactionContext {
                            snapshot: session.context.snapshot(),
                            phase: CompactionPhase::MidTurn,
                        },
                    )
                    .await?;
                    continue;
                }
                if let Some(message) = final_message {
                    return Ok(if message.trim().is_empty() {
                        "The model completed without emitting assistant text.".to_owned()
                    } else {
                        message
                    });
                }
                return Err(NanocodexError::MalformedResponse {
                    detail: "model completed without a final message or exec call",
                });
            }

            session.conversation.clear_delta();
            let history = code_calls
                .iter()
                .any(|call| call.name == "exec")
                .then(|| Arc::new(session.conversation.flattened_history()));
            self.execute_model_tools(
                &session.tools,
                &mut session.conversation,
                call_index,
                code_calls,
                history,
            )
            .await?;
            let compacted = self
                .maybe_compact(
                    call_index,
                    &mut session.conversation,
                    &session.factory,
                    CompactionContext {
                        snapshot: session.context.snapshot(),
                        phase: CompactionPhase::MidTurn,
                    },
                )
                .await?;
            // Codex resumes a model/tool continuation immediately after
            // compaction, then drains steering at the following boundary.
            can_drain_steers = !compacted;
        }
    }

    async fn drain_steers(
        &mut self,
        conversation: &mut ConversationState,
        steers: &mut tokio::sync::mpsc::Receiver<Prompt>,
    ) -> Result<()> {
        while let Ok(steer) = steers.try_recv() {
            if trace_content_enabled()
                && let Ok(content) = serde_json::to_string(&steer)
            {
                info!(
                    target: "nanocodex",
                    content_kind = "steer",
                    content = content.as_str(),
                    "turn content"
                );
            }
            let instruction_bytes = steer.instruction.text_bytes();
            let user_content = prepare_user_input(&steer.instruction).await;
            conversation.append([ResponseItem::message(MessageRole::User, user_content)]);
            self.stats.steers += 1;
            self.events.emit(
                AgentEventKind::RunSteered,
                RunSteered {
                    steer_index: self.stats.steers,
                    instruction_bytes,
                },
            )?;
        }
        Ok(())
    }

    async fn execute_model_tools(
        &mut self,
        tools: &ToolRuntime,
        conversation: &mut ConversationState,
        call_index: u32,
        calls: Vec<CodeCall>,
        history: Option<Arc<Vec<ResponseItem>>>,
    ) -> Result<()> {
        self.active_tool_batch_started_at = Some(Instant::now());
        let mut prepared = Vec::with_capacity(calls.len());
        for call in calls {
            let active = self.prepare_model_tool_call(call_index, &call)?;
            let supports_parallel = tools.supports_parallel_tool_calls(&qualified_tool_name(&call));
            prepared.push((call, supports_parallel, active));
        }

        let gate = Arc::new(RwLock::new(()));
        let events = self.events.clone();
        let tool_call_indices = self.tool_call_indices.clone();
        let session_id = events.request_id().to_owned();
        let mut executions = prepared
            .into_iter()
            .map(|(call, supports_parallel, active)| {
                let gate = Arc::clone(&gate);
                let history = history.clone();
                let events = events.clone();
                let tool_call_indices = tool_call_indices.clone();
                let session_id = session_id.clone();
                async move {
                    let started_at = active.started_at;
                    let dispatch = async {
                        if supports_parallel {
                            let _guard = gate.read().await;
                            active
                                .execution_started_at
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .replace(Instant::now());
                            Self::execute_model_tool_call(
                                tools,
                                &events,
                                &tool_call_indices,
                                call_index,
                                call,
                                history,
                                &session_id,
                                started_at,
                                &active.progress,
                                &active.span,
                            )
                            .await
                        } else {
                            let _guard = gate.write().await;
                            active
                                .execution_started_at
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .replace(Instant::now());
                            Self::execute_model_tool_call(
                                tools,
                                &events,
                                &tool_call_indices,
                                call_index,
                                call,
                                history,
                                &session_id,
                                started_at,
                                &active.progress,
                                &active.span,
                            )
                            .await
                        }
                    };
                    let result = match AssertUnwindSafe(dispatch).catch_unwind().await {
                        Ok(result) => result,
                        Err(payload) => Ok(Self::panicked_tool_call(&active, payload)),
                    };
                    match result {
                        Ok(mut completed) => {
                            completed.work_duration_ns =
                                Self::completed_tool_work_duration(&active);
                            Self::emit_completed_tool_result(&events, &completed)?;
                            active
                                .completion
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .replace(completed);
                            Ok(active)
                        }
                        Err(error) => Err(error),
                    }
                }
            })
            .collect::<FuturesOrdered<_>>();
        while let Some(active) = executions.next().await {
            let active = active?;
            let Some(completed) = active
                .completion
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            else {
                return Err(NanocodexError::InvalidAttemptState {
                    detail: "completed tool call did not retain its output",
                });
            };
            let Some(active_index) = self
                .active_tool_calls
                .iter()
                .position(|candidate| Arc::ptr_eq(&candidate.completion, &active.completion))
            else {
                return Err(NanocodexError::InvalidAttemptState {
                    detail: "completed tool call was not active",
                });
            };
            self.active_tool_calls.remove(active_index);
            conversation.append(self.finish_completed_tool_call(completed, &active.progress)?);
        }
        self.finish_active_tool_batch_wall();
        Ok(())
    }

    fn prepare_model_tool_call(
        &mut self,
        call_index: u32,
        call: &CodeCall,
    ) -> Result<ActiveToolCall> {
        self.tool_call_indices
            .insert(call.call_id.clone().into_boxed_str(), call_index);
        let qualified_name = qualified_tool_name(call);
        let arguments = if call.name == "exec" {
            ToolCallArguments::Text(&call.input)
        } else {
            serde_json::from_str::<&RawValue>(&call.input)
                .map_or(ToolCallArguments::Text(&call.input), ToolCallArguments::Raw)
        };
        self.events.emit(
            AgentEventKind::ToolCall,
            ToolCallEvent {
                call_id: &call.call_id,
                tool: &qualified_name,
                arguments,
                model_call_index: call_index,
            },
        )?;
        self.stats.tool_calls += 1;
        let started_at = Instant::now();
        let span = model_tool_span(call, call_index);
        record_span_content(&span, "tool.arguments", &call.input);
        let active = ActiveToolCall {
            call_id: call.call_id.clone(),
            name: qualified_name,
            kind: call.kind,
            started_at,
            shell_abort_format: call.namespace.is_none()
                && matches!(call.name.as_str(), "shell_command" | "unified_exec"),
            completion: Arc::new(Mutex::new(None)),
            progress: Arc::new(Mutex::new(ActiveToolProgress::default())),
            execution_started_at: Arc::new(Mutex::new(None)),
            span,
        };
        self.active_tool_calls.push(active.clone());
        Ok(active)
    }

    fn finish_completed_tool_call(
        &mut self,
        completed: CompletedToolCall,
        progress: &Mutex<ActiveToolProgress>,
    ) -> Result<Vec<ResponseItem>> {
        self.stats.tool_work_duration_ns += completed.work_duration_ns;
        self.finish_active_tool_progress(progress);
        Ok(completed.response_items)
    }

    fn finish_active_tool_progress(&mut self, progress: &Mutex<ActiveToolProgress>) {
        let progress = std::mem::take(
            &mut *progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        self.stats.tool_calls += progress.nested_tool_calls;
    }

    fn completed_tool_work_duration(active: &ActiveToolCall) -> u64 {
        active
            .execution_started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map_or(0, |started_at| elapsed_ns(*started_at))
    }

    fn finish_cancelled_tool_work(&mut self, active: &ActiveToolCall) {
        let started_at = active
            .execution_started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(started_at) = started_at {
            self.stats.tool_work_duration_ns += elapsed_ns(started_at);
        }
    }

    fn finish_active_tool_batch_wall(&mut self) {
        if let Some(started_at) = self.active_tool_batch_started_at.take() {
            self.stats.tool_wall_duration_ns += elapsed_ns(started_at);
        }
    }

    fn emit_completed_tool_result(events: &EventSink, completed: &CompletedToolCall) -> Result<()> {
        events.emit(
            AgentEventKind::ToolResult,
            ToolResultEvent {
                call_id: &completed.call_id,
                tool: &completed.tool,
                status: status(completed.success),
                duration_ns: completed.duration_ns,
                started_after_ns: None,
                result: &completed.output,
                metadata: completed.metadata.as_deref(),
            },
        )?;
        Ok(())
    }

    fn panicked_tool_call(
        active: &ActiveToolCall,
        payload: Box<dyn Any + Send>,
    ) -> CompletedToolCall {
        let message = panic_payload(payload);
        record_span_content(&active.span, "tool.panic", &message);
        let output = ToolOutputBody::Text("aborted".to_owned());
        let duration_ns = elapsed_ns(active.started_at);
        record_tool_span_terminal(&active.span, "failed", "ERROR", duration_ns, &output);
        let response_item = match active.kind {
            CodeCallKind::Custom => custom_tool_output(active.call_id.clone(), output.clone()),
            CodeCallKind::Function => function_tool_output(active.call_id.clone(), output.clone()),
            CodeCallKind::ToolSearch => tool_search_output(active.call_id.clone(), Vec::new()),
        };
        CompletedToolCall {
            call_id: active.call_id.clone(),
            tool: active.name.clone(),
            success: false,
            duration_ns,
            work_duration_ns: Self::completed_tool_work_duration(active),
            output,
            metadata: None,
            response_items: vec![response_item],
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_model_tool_call(
        tools: &ToolRuntime,
        events: &EventSink,
        tool_call_indices: &HashMap<Box<str>, u32>,
        call_index: u32,
        call: CodeCall,
        history: Option<Arc<Vec<ResponseItem>>>,
        session_id: &str,
        started_at: Instant,
        progress: &Mutex<ActiveToolProgress>,
        tool_span: &tracing::Span,
    ) -> Result<CompletedToolCall> {
        let qualified_name = qualified_tool_name(&call);
        if let Some(message) = unsupported_tool_message(tools, &call) {
            let output = ToolOutputBody::Text(message);
            record_tool_span_terminal(tool_span, "failed", "ERROR", 0, &output);
            let response_item = match call.kind {
                CodeCallKind::Custom => custom_tool_output(call.call_id.clone(), output.clone()),
                CodeCallKind::Function => {
                    function_tool_output(call.call_id.clone(), output.clone())
                }
                CodeCallKind::ToolSearch => tool_search_output(call.call_id.clone(), Vec::new()),
            };
            return Ok(CompletedToolCall {
                call_id: call.call_id,
                tool: qualified_name,
                success: false,
                duration_ns: 0,
                work_duration_ns: 0,
                output,
                metadata: None,
                response_items: vec![response_item],
            });
        }
        if matches!(call.kind, CodeCallKind::Function) && call.namespace.is_some() {
            let context = ToolContext::new(
                MODEL,
                session_id,
                &call.call_id,
                &[],
                DEFAULT_TOOL_OUTPUT_TOKENS,
            );
            let execution = match RawValue::from_string(call.input.clone()) {
                Ok(input) => {
                    tools
                        .execute_tool(&qualified_name, ToolInput::Function(input), context)
                        .instrument(tool_span.clone())
                        .await
                }
                Err(error) => ToolOutput::error(format!(
                    "failed to encode {qualified_name} arguments: {error}"
                )),
            };
            if let Some(content) = serialize_trace_content(&execution.output) {
                record_span_content(tool_span, "tool.output", &content);
            }
            let duration_ns = elapsed_ns(started_at);
            tool_span.record("status", status(execution.success));
            tool_span.record("otel.status_code", otel_status(execution.success));
            tool_span.record("duration_ns", duration_ns);
            return Ok(CompletedToolCall {
                call_id: call.call_id.clone(),
                tool: qualified_name,
                success: execution.success,
                duration_ns,
                work_duration_ns: 0,
                response_items: vec![function_tool_output(call.call_id, execution.output.clone())],
                output: execution.output,
                metadata: execution.metadata,
            });
        }
        if matches!(call.kind, CodeCallKind::ToolSearch) {
            let search_history = history.as_deref().map_or(&[][..], Vec::as_slice);
            let context = ToolContext::new(
                MODEL,
                session_id,
                &call.call_id,
                search_history,
                DEFAULT_TOOL_OUTPUT_TOKENS,
            );
            let execution = match RawValue::from_string(call.input.clone()) {
                Ok(input) => {
                    tools
                        .execute_tool("tool_search", ToolInput::Function(input), context)
                        .instrument(tool_span.clone())
                        .await
                }
                Err(error) => {
                    ToolOutput::error(format!("failed to encode tool_search arguments: {error}"))
                }
            };
            if let Some(content) = serialize_trace_content(&execution.output) {
                record_span_content(tool_span, "tool.output", &content);
            }
            let duration_ns = elapsed_ns(started_at);
            tool_span.record("status", status(execution.success));
            tool_span.record("otel.status_code", otel_status(execution.success));
            tool_span.record("duration_ns", duration_ns);
            let tools = if execution.success {
                match execution.code_mode_value() {
                    Value::Array(tools) => tools,
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            return Ok(CompletedToolCall {
                call_id: call.call_id.clone(),
                tool: qualified_name,
                success: execution.success,
                duration_ns,
                work_duration_ns: 0,
                response_items: vec![tool_search_output(call.call_id, tools)],
                output: execution.output,
                metadata: execution.metadata,
            });
        }
        let owned_context = owned_code_context(&call, history, session_id)?;
        let mut observer = NestedToolEventObserver {
            events,
            tool_call_indices,
            progress,
            fallback_call_index: call_index,
            parent_call_id: &call.call_id,
            error: None,
        };
        let mut execution = execute_code_call(
            tools,
            &call,
            owned_context,
            session_id,
            &mut observer,
            tool_span,
        )
        .await;
        let update_error = observer.error.take();
        drop(observer);
        if let Some(error) = update_error {
            return Err(error);
        }
        prepare_output_images(&mut execution.output).await;
        if let Some(content) = serialize_trace_content(&execution.output) {
            record_span_content(tool_span, "tool.output", &content);
        }
        let duration_ns = elapsed_ns(started_at);
        tool_span.record("status", status(execution.success));
        tool_span.record("otel.status_code", otel_status(execution.success));
        tool_span.record("duration_ns", duration_ns);
        let output = match call.kind {
            CodeCallKind::Custom => {
                custom_tool_output(call.call_id.clone(), execution.output.clone())
            }
            CodeCallKind::Function => {
                function_tool_output(call.call_id.clone(), execution.output.clone())
            }
            CodeCallKind::ToolSearch => {
                unreachable!("native tool_search returned through Code Mode")
            }
        };
        let mut outputs = Vec::with_capacity(execution.notifications.len() + 1);
        outputs.push(output);
        outputs.extend(
            execution.notifications.into_iter().map(|notification| {
                custom_tool_notification(notification.call_id, notification.text)
            }),
        );
        Ok(CompletedToolCall {
            call_id: call.call_id,
            tool: qualified_name,
            success: execution.success,
            duration_ns,
            work_duration_ns: 0,
            output: execution.output,
            metadata: None,
            response_items: outputs,
        })
    }

    async fn maybe_compact(
        &mut self,
        after_model_call_index: u32,
        conversation: &mut ConversationState,
        factory: &ResponsesAttemptFactory,
        context: CompactionContext<'_>,
    ) -> Result<bool> {
        let CompactionContext { snapshot, phase } = context;
        let Some(auto_compact_token_limit) = compaction::auto_compact_token_limit(MODEL) else {
            return Ok(false);
        };
        let active_context_tokens = conversation.active_context_tokens();
        if !self.force_compaction && active_context_tokens < auto_compact_token_limit {
            return Ok(false);
        }
        let previous_response_id = conversation.previous_response_id();
        let (item, _usage, server_reasoning_included) = self
            .perform_compaction(
                after_model_call_index,
                conversation.prompt_history(),
                conversation.delta_start(),
                previous_response_id,
                active_context_tokens,
                auto_compact_token_limit,
                factory,
            )
            .await?;
        conversation.observe_server_reasoning(server_reasoning_included);
        match phase {
            CompactionPhase::PreTurn => {
                conversation.install_pre_turn_compaction(item, factory.profile().prefix());
            }
            CompactionPhase::MidTurn => {
                let snapshot = snapshot.ok_or(NanocodexError::InvalidAttemptState {
                    detail: "mid-turn compaction is missing its context snapshot",
                })?;
                let canonical_context = snapshot.full_item();
                conversation.install_mid_turn_compaction(
                    item,
                    developer_context(),
                    canonical_context,
                    factory.profile().prefix(),
                );
            }
        }
        self.force_compaction = false;
        Ok(true)
    }

    async fn perform_warmup(&mut self, factory: &ResponsesAttemptFactory) -> Result<WarmupOutcome> {
        if matches!(self.config.responses_transport, ResponsesTransport::Https) {
            return Ok(WarmupOutcome {
                response_id: None,
                server_reasoning_included: false,
            });
        }
        let started_at = Instant::now();
        self.events.emit(
            AgentEventKind::ModelWarmupStarted,
            WarmupStarted {
                model: MODEL,
                prompt_cache_key: factory.profile().prompt_cache_key(),
            },
        )?;
        let span = warmup_span(&self.config);
        if let Some(content) = serialize_trace_content(factory.profile().prefix()) {
            record_span_content(&span, "model.input", &content);
        }
        let shared_prompt_cache = self.prompt_cache.shared().cloned();
        let outcome = if let Some(cache) = shared_prompt_cache {
            match cache.entry(factory.profile()).await {
                Ok(entry) => {
                    let mut execution = None;
                    let initialized = entry
                        .get_or_try_init(|| async {
                            let completed = self.execute_warmup(factory, &span).await?;
                            execution = Some(completed);
                            Ok(())
                        })
                        .await;
                    initialized.map(|()| execution)
                }
                Err(error) => Err(error),
            }
        } else {
            self.execute_warmup(factory, &span).await.map(Some)
        };
        let execution = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                span.record("status", "failed");
                span.record("otel.status_code", "ERROR");
                span.record("duration_ns", elapsed_ns(started_at));
                return self.warmup_failed(started_at, error);
            }
        };
        let duration_ns = elapsed_ns(started_at);
        let (response_id, source, attempt, connection_generation, usage, server_reasoning_included) =
            if let Some(execution) = execution {
                if let Some(usage) = &execution.usage {
                    self.stats.warmup_usage.add(usage);
                }
                (
                    Some(execution.response_id),
                    "response",
                    Some(execution.attempt),
                    Some(execution.connection_generation),
                    execution.usage,
                    execution.server_reasoning_included,
                )
            } else {
                (None, "shared_prefix", None, None, None, false)
            };
        span.record("warmup.source", source);
        if let Some(usage) = &usage {
            record_usage(&span, usage, self.fast_mode);
        }
        span.record("status", "completed");
        span.record("otel.status_code", "OK");
        span.record("duration_ns", duration_ns);
        self.stats.warmup_duration_ns += duration_ns;
        self.stats.last_response_id.clone_from(&response_id);
        self.events.emit(
            AgentEventKind::ModelWarmupCompleted,
            WarmupCompleted {
                response_id: response_id.as_deref(),
                source,
                attempt,
                connection_generation,
                duration_ns,
                usage: usage.as_ref(),
            },
        )?;
        Ok(WarmupOutcome {
            response_id,
            server_reasoning_included,
        })
    }

    async fn execute_warmup(
        &mut self,
        factory: &ResponsesAttemptFactory,
        span: &tracing::Span,
    ) -> Result<WarmupExecution> {
        let success = self
            .client
            .execute(factory.warmup(self.thinking, self.fast_mode))
            .instrument(span.clone())
            .await
            .map_err(Into::into)?;
        let attempt = success.attempt();
        let connection_generation = success.connection_generation();
        let server_reasoning_included = success.server_reasoning_included();
        let ResponsesOutput::Warmup(response) = success.into_output() else {
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            return Err(NanocodexError::InvalidAttemptState {
                detail: "warmup returned a non-warmup response",
            });
        };
        Ok(WarmupExecution {
            response_id: response.id,
            attempt,
            connection_generation,
            usage: response.usage,
            server_reasoning_included,
        })
    }

    fn warmup_failed<T>(&mut self, started_at: Instant, error: NanocodexError) -> Result<T> {
        let duration_ns = elapsed_ns(started_at);
        self.stats.warmup_duration_ns += duration_ns;
        let message = error.to_string();
        self.events.emit(
            AgentEventKind::ModelWarmupFailed,
            WarmupFailed {
                duration_ns,
                error: &message,
            },
        )?;
        Err(error)
    }

    async fn perform_model_call(
        &mut self,
        call_index: u32,
        conversation: &mut ConversationState,
        factory: &ResponsesAttemptFactory,
    ) -> Result<TurnResult> {
        let (prompt_history, prompt_repaired) = conversation.prompt_history_with_repair();
        let previous_response_id = if prompt_repaired {
            None
        } else {
            conversation.previous_response_id().map(str::to_owned)
        };
        let started_at = Instant::now();
        self.stats.model_calls += 1;
        self.events.emit(
            AgentEventKind::ModelCallStarted,
            ModelCallStarted {
                call_index,
                model: MODEL,
                reasoning_mode: self.config.reasoning_mode.as_str(),
                effort: self.thinking.as_str(),
                previous_response_id: previous_response_id.as_deref(),
            },
        )?;
        let request = factory.generation(
            call_index,
            prompt_history.clone(),
            conversation.shared_history(),
            conversation.delta_start(),
            previous_response_id.as_deref(),
            self.thinking,
            self.fast_mode,
        );
        let (input_item_count, input_bytes, input_content) = trace_model_input(&request);
        let span = model_call_span(
            call_index,
            self.config.reasoning_mode.as_str(),
            self.thinking.as_str(),
            previous_response_id.is_some(),
            input_item_count,
            input_bytes,
        );
        if let Some(input_content) = &input_content {
            record_span_content(&span, "model.input", input_content);
        }
        let success = match self.client.execute(request).instrument(span.clone()).await {
            Ok(success) => success,
            Err(error) => {
                span.record("status", "failed");
                span.record("otel.status_code", "ERROR");
                span.record("duration_ns", elapsed_ns(started_at));
                return self.model_call_failed(call_index, started_at, error.into());
            }
        };
        let attempt = success.attempt();
        let connection_generation = success.connection_generation();
        conversation.observe_server_reasoning(success.server_reasoning_included());
        let ResponsesOutput::Generation(response) = success.into_output() else {
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            return Err(NanocodexError::InvalidAttemptState {
                detail: "generation returned a non-generation response",
            });
        };
        if prompt_repaired {
            conversation.adopt_prompt_history(prompt_history);
        }
        let duration_ns = elapsed_ns(started_at);
        record_model_response(&span, &response);
        span.record("status", "completed");
        span.record("otel.status_code", "OK");
        span.record("duration_ns", duration_ns);
        if let Some(usage) = &response.usage {
            record_usage(&span, usage, self.fast_mode);
        }
        self.stats.model_duration_ns += duration_ns;
        if let Some(usage) = &response.usage {
            self.stats.usage.add(usage);
        }
        self.stats.last_response_id = Some(response.id.clone());
        self.events.emit(
            AgentEventKind::ModelCallCompleted,
            ModelCallCompleted {
                call_index,
                model: MODEL,
                response_id: &response.id,
                attempt,
                connection_generation,
                status: &response.status,
                duration_ns,
                time_to_first_event_ns: response.time_to_first_event_ns,
                time_to_first_output_ns: response.time_to_first_output_ns,
                tool_calls: response.code_calls.len(),
                usage: response.usage.as_ref(),
            },
        )?;
        Ok(response)
    }

    #[allow(clippy::too_many_arguments)]
    async fn perform_compaction(
        &mut self,
        after_model_call_index: u32,
        history: nanocodex_oai_api::responses::ResponseHistory,
        incremental_start: usize,
        previous_response_id: Option<&str>,
        active_context_tokens: u64,
        auto_compact_token_limit: u64,
        factory: &ResponsesAttemptFactory,
    ) -> Result<(ResponseItem, Option<Usage>, bool)> {
        let trigger = compaction::trigger();
        let mut history = history;
        compaction::trim_tool_outputs_to_fit_context_window(
            &mut history,
            factory.profile().prefix(),
        );
        let started_at = Instant::now();
        self.stats.compactions += 1;
        self.events.emit(
            AgentEventKind::ModelCompactionStarted,
            CompactionStarted {
                after_model_call_index,
                active_context_tokens,
                auto_compact_token_limit,
                previous_response_id,
            },
        )?;
        let request = factory.compaction(
            after_model_call_index,
            history.clone(),
            history,
            incremental_start,
            previous_response_id,
            trigger,
            self.thinking,
            self.fast_mode,
        );
        let (input_item_count, input_bytes, input_content) = trace_model_input(&request);
        let span = compaction_span(after_model_call_index, input_item_count, input_bytes);
        if let Some(input_content) = &input_content {
            record_span_content(&span, "model.input", input_content);
        }
        let success = match self.client.execute(request).instrument(span.clone()).await {
            Ok(success) => success,
            Err(error) => {
                span.record("status", "failed");
                span.record("otel.status_code", "ERROR");
                span.record("duration_ns", elapsed_ns(started_at));
                return self.compaction_failed(after_model_call_index, started_at, error.into());
            }
        };
        let attempt = success.attempt();
        let connection_generation = success.connection_generation();
        let server_reasoning_included = success.server_reasoning_included();
        let ResponsesOutput::Compaction(response) = success.into_output() else {
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            return Err(NanocodexError::InvalidAttemptState {
                detail: "compaction returned a non-compaction response",
            });
        };
        let duration_ns = elapsed_ns(started_at);
        span.record("model.response.id", response.id.as_str());
        if let Some(content) = serialize_trace_content(&response.item) {
            record_span_content(&span, "model.output_item", &content);
        }
        span.record("status", "completed");
        span.record("otel.status_code", "OK");
        span.record("duration_ns", duration_ns);
        self.stats.model_duration_ns += duration_ns;
        if let Some(usage) = &response.usage {
            record_usage(&span, usage, self.fast_mode);
            self.stats.usage.add(usage);
        }
        self.stats.last_response_id = Some(response.id.clone());
        self.events.emit(
            AgentEventKind::ModelCompactionCompleted,
            CompactionCompleted {
                after_model_call_index,
                response_id: &response.id,
                attempt,
                connection_generation,
                status: &response.status,
                duration_ns,
                time_to_first_event_ns: response.time_to_first_event_ns,
                time_to_first_output_ns: response.time_to_first_output_ns,
                usage: response.usage.as_ref(),
            },
        )?;
        Ok((response.item, response.usage, server_reasoning_included))
    }

    fn compaction_failed<T>(
        &mut self,
        after_model_call_index: u32,
        started_at: Instant,
        error: crate::NanocodexError,
    ) -> Result<T> {
        let duration_ns = elapsed_ns(started_at);
        self.stats.model_duration_ns += duration_ns;
        let message = error.to_string();
        self.events.emit(
            AgentEventKind::ModelCompactionFailed,
            CompactionFailed {
                after_model_call_index,
                duration_ns,
                error: &message,
            },
        )?;
        Err(error)
    }

    fn model_call_failed<T>(
        &mut self,
        call_index: u32,
        started_at: Instant,
        error: crate::NanocodexError,
    ) -> Result<T> {
        let duration_ns = elapsed_ns(started_at);
        self.stats.model_duration_ns += duration_ns;
        let message = error.to_string();
        self.events.emit(
            AgentEventKind::ModelCallFailed,
            ModelCallFailed {
                call_index,
                model: MODEL,
                duration_ns,
                error: &message,
            },
        )?;
        Err(error)
    }
}

fn unsupported_tool_message(tools: &ToolRuntime, call: &CodeCall) -> Option<String> {
    if call.namespace.is_none() && matches!(call.name.as_str(), "exec" | "wait") {
        return None;
    }
    if call.namespace.is_some() && matches!(call.kind, CodeCallKind::Function) {
        let qualified_name = qualified_tool_name(call);
        return (!tools.contains(&qualified_name))
            .then(|| format!("unsupported call: {qualified_name}"));
    }
    let qualified_name = qualified_tool_name(call);
    Some(match &call.kind {
        CodeCallKind::Custom => format!("unsupported custom tool call: {qualified_name}"),
        CodeCallKind::Function => format!("unsupported call: {qualified_name}"),
        CodeCallKind::ToolSearch => return None,
    })
}

fn qualified_tool_name(call: &CodeCall) -> String {
    format!("{}{}", call.namespace.as_deref().unwrap_or(""), call.name)
}

fn trace_model_input(request: &ResponsesAttempt) -> (usize, usize, Option<String>) {
    let item_count = request.input_item_count();
    if !trace_content_enabled() {
        return (item_count, 0, None);
    }
    let items = request.input_items().collect::<Vec<_>>();
    let content = serde_json::to_string(&items).ok();
    let bytes = content.as_ref().map_or(0, String::len);
    (item_count, bytes, content)
}

fn trace_content_enabled() -> bool {
    tracing::enabled!(target: "nanocodex", tracing::Level::INFO)
}

fn serialize_trace_content<T: Serialize + ?Sized>(value: &T) -> Option<String> {
    trace_content_enabled()
        .then(|| serde_json::to_string(value).ok())
        .flatten()
}

fn record_tool_span_terminal(
    span: &tracing::Span,
    status: &'static str,
    otel_status: &'static str,
    duration_ns: u64,
    output: &ToolOutputBody,
) {
    if let Some(content) = serialize_trace_content(output) {
        record_span_content(span, "tool.output", &content);
    }
    span.record("status", status);
    span.record("otel.status_code", otel_status);
    span.record("duration_ns", duration_ns);
}

fn panic_payload(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => payload.downcast::<&'static str>().map_or_else(
            |_| "non-string panic payload".to_owned(),
            |message| (*message).to_owned(),
        ),
    }
}

fn model_tool_span(call: &CodeCall, call_index: u32) -> tracing::Span {
    let qualified_name = qualified_tool_name(call);
    info_span!(
        target: "nanocodex",
        "tool.call",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        tool.name = %qualified_name,
        tool.call_id = %call.call_id,
        tool.arguments.bytes = call.input.len(),
        model.call_index = call_index,
        status = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
    )
}

fn owned_code_context(
    call: &CodeCall,
    history: Option<Arc<Vec<ResponseItem>>>,
    session_id: &str,
) -> Result<Option<OwnedToolContext>> {
    if call.name != "exec" {
        return Ok(None);
    }
    let history = history.ok_or(NanocodexError::MalformedResponse {
        detail: "exec call did not have an owned history snapshot",
    })?;
    Ok(Some(OwnedToolContext::new(
        MODEL,
        session_id,
        &call.call_id,
        history,
        DEFAULT_TOOL_OUTPUT_TOKENS,
    )))
}

fn record_span_content(span: &tracing::Span, kind: &'static str, content: &str) {
    span.in_scope(|| {
        info!(
            target: "nanocodex",
            content_kind = kind,
            content,
            "trace content"
        );
    });
}

fn record_indexed_span_content(
    span: &tracing::Span,
    kind: &'static str,
    index: usize,
    content: &str,
) {
    span.in_scope(|| {
        info!(
            target: "nanocodex",
            content_kind = kind,
            output.index = index,
            content,
            "trace content"
        );
    });
}

fn model_call_span(
    call_index: u32,
    reasoning_mode: &str,
    reasoning_effort: &str,
    previous_response: bool,
    input_item_count: usize,
    input_bytes: usize,
) -> tracing::Span {
    info_span!(
        target: "nanocodex",
        "model.call",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        model = MODEL,
        reasoning.mode = reasoning_mode,
        reasoning.effort = reasoning_effort,
        model.call_index = call_index,
        previous_response,
        model.input.item_count = input_item_count,
        model.input.bytes = input_bytes,
        model.response.id = tracing::field::Empty,
        model.response.status = tracing::field::Empty,
        model.response.end_turn = tracing::field::Empty,
        model.output.item_count = tracing::field::Empty,
        model.output.bytes = tracing::field::Empty,
        model.tool_call_count = tracing::field::Empty,
        assistant.output.bytes = tracing::field::Empty,
        status = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
        input_tokens = tracing::field::Empty,
        cached_input_tokens = tracing::field::Empty,
        cache_write_input_tokens = tracing::field::Empty,
        output_tokens = tracing::field::Empty,
        reasoning_output_tokens = tracing::field::Empty,
        total_tokens = tracing::field::Empty,
        cost.usd = tracing::field::Empty,
        cost.service_tier = tracing::field::Empty,
        reasoning.summary_count = tracing::field::Empty,
        time_to_first_event_ns = tracing::field::Empty,
        time_to_first_output_ns = tracing::field::Empty,
        stream.display_delta.count = tracing::field::Empty,
        stream.display_delta.bytes = tracing::field::Empty,
        stream.inter_delta_gap.max_ns = tracing::field::Empty,
        stream.inter_delta_stall_100ms.count = tracing::field::Empty,
    )
}

fn warmup_span(config: &ModelConfig) -> tracing::Span {
    info_span!(
        target: "nanocodex",
        "model.warmup",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        model = MODEL,
        system_prompt.bytes = config.system_prompt().len(),
        warmup.source = tracing::field::Empty,
        status = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
        input_tokens = tracing::field::Empty,
        cached_input_tokens = tracing::field::Empty,
        cache_write_input_tokens = tracing::field::Empty,
        output_tokens = tracing::field::Empty,
        reasoning_output_tokens = tracing::field::Empty,
        total_tokens = tracing::field::Empty,
        cost.usd = tracing::field::Empty,
        cost.service_tier = tracing::field::Empty,
    )
}

fn compaction_span(
    after_model_call_index: u32,
    input_item_count: usize,
    input_bytes: usize,
) -> tracing::Span {
    info_span!(
        target: "nanocodex",
        "model.compaction",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        after_model_call_index,
        model.input.item_count = input_item_count,
        model.input.bytes = input_bytes,
        model.response.id = tracing::field::Empty,
        status = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
        input_tokens = tracing::field::Empty,
        cached_input_tokens = tracing::field::Empty,
        cache_write_input_tokens = tracing::field::Empty,
        output_tokens = tracing::field::Empty,
        reasoning_output_tokens = tracing::field::Empty,
        total_tokens = tracing::field::Empty,
        cost.usd = tracing::field::Empty,
        cost.service_tier = tracing::field::Empty,
    )
}

fn record_usage(span: &tracing::Span, usage: &Usage, fast_mode: bool) {
    let cached_input_tokens = usage
        .input_tokens_details
        .as_ref()
        .map_or(0, |details| details.cached_tokens);
    let cache_write_input_tokens = usage
        .input_tokens_details
        .as_ref()
        .map_or(0, |details| details.cache_write_tokens);
    let reasoning_output_tokens = usage
        .output_tokens_details
        .as_ref()
        .map_or(0, |details| details.reasoning_tokens);
    span.record("input_tokens", usage.input_tokens);
    span.record("cached_input_tokens", cached_input_tokens);
    span.record("cache_write_input_tokens", cache_write_input_tokens);
    span.record("output_tokens", usage.output_tokens);
    span.record("reasoning_output_tokens", reasoning_output_tokens);
    span.record("total_tokens", usage.total_tokens);
    let estimate = estimate(
        usage,
        if fast_mode {
            ServiceTier::Priority
        } else {
            ServiceTier::Standard
        },
    );
    let amount = estimate.amount().decimal();
    span.record("cost.usd", amount.as_str());
    span.record("cost.service_tier", estimate.service_tier().as_str());
}

fn record_turn_usage(span: &tracing::Span, usage: &TurnUsage) {
    span.record("usage.input_tokens", usage.input_tokens());
    span.record("usage.cached_input_tokens", usage.cached_input_tokens());
    span.record(
        "usage.cache_write_input_tokens",
        usage.cache_write_input_tokens(),
    );
    span.record("usage.output_tokens", usage.output_tokens());
    span.record(
        "usage.reasoning_output_tokens",
        usage.reasoning_output_tokens(),
    );
    span.record("usage.total_tokens", usage.total_tokens());
    span.record("cost.status", usage.cost_status().as_str());
    if let Some(cost) = usage.estimated_cost() {
        let amount = cost.amount().decimal();
        span.record("cost.usd", amount.as_str());
        span.record("cost.service_tier", cost.service_tier().as_str());
    }
}

fn record_model_response(span: &tracing::Span, response: &TurnResult) {
    span.record("model.response.id", response.id.as_str());
    span.record("model.response.status", response.status.as_str());
    if let Some(end_turn) = response.end_turn {
        span.record("model.response.end_turn", end_turn);
    }
    span.record("model.output.item_count", response.output_items.len());
    span.record("model.tool_call_count", response.code_calls.len());
    let trace_content = trace_content_enabled();
    let mut output_bytes = usize::from(trace_content).saturating_mul(2);
    let mut serialized_items = 0_usize;
    let mut summary_count = 0_usize;
    for (index, item) in response.output_items.iter().enumerate() {
        let kind = if let ResponseItem::Reasoning { summary, .. } = item {
            summary_count = summary_count.saturating_add(summary.len());
            "reasoning"
        } else {
            "model.output_item"
        };
        if trace_content && let Ok(content) = serde_json::to_string(item) {
            output_bytes = output_bytes
                .saturating_add(usize::from(serialized_items != 0))
                .saturating_add(content.len());
            serialized_items = serialized_items.saturating_add(1);
            record_indexed_span_content(span, kind, index, &content);
        }
    }
    span.record("model.output.bytes", output_bytes);
    if let Some(message) = &response.final_message {
        span.record("assistant.output.bytes", message.len());
    }
    span.record("reasoning.summary_count", summary_count);
    span.record("time_to_first_event_ns", response.time_to_first_event_ns);
    if let Some(time_to_first_output_ns) = response.time_to_first_output_ns {
        span.record("time_to_first_output_ns", time_to_first_output_ns);
    }
    span.record(
        "stream.display_delta.count",
        response.pipeline_stats.display_delta_count,
    );
    span.record(
        "stream.display_delta.bytes",
        response.pipeline_stats.display_delta_bytes,
    );
    span.record(
        "stream.inter_delta_gap.max_ns",
        response.pipeline_stats.inter_delta_gap_max_ns,
    );
    span.record(
        "stream.inter_delta_stall_100ms.count",
        response.pipeline_stats.inter_delta_stall_100ms_count,
    );
}

fn request_profile(
    session_id: &str,
    prompt_cache_key: &str,
    tool_specs: Vec<ToolDefinition>,
    system_prompt: &str,
) -> RequestProfile {
    let mut prefix = [
        ResponseItem::additional_tools(tool_specs),
        ResponseItem::message(
            MessageRole::Developer,
            [ContentItem::InputText {
                text: system_prompt.into(),
            }],
        ),
    ];
    assign_request_prefix_ids(&mut prefix);
    RequestProfile::new(session_id, prompt_cache_key, Arc::from(prefix))
}

fn assign_request_prefix_ids(prefix: &mut [ResponseItem]) {
    for item in prefix {
        // Responses Lite request-prefix items are transport configuration, not
        // retained conversation. Codex sends both without client-defined IDs.
        if matches!(
            item,
            ResponseItem::AdditionalTools { .. }
                | ResponseItem::Message {
                    role: MessageRole::Developer,
                    ..
                }
        ) {
            item.strip_id();
            continue;
        }
        if item.id().is_some_and(|id| !id.is_empty()) {
            continue;
        }
        assign_missing_response_item_id(item);
    }
}

fn attempt_factory(
    events: &EventSink,
    transport_stats: &Arc<TransportStats>,
    prompt_cache_key: &str,
    tools: &ToolRuntime,
    system_prompt: &str,
) -> ResponsesAttemptFactory {
    let tool_specs = tools.model_specs(events.request_id());
    ResponsesAttemptFactory::new(
        request_profile(
            events.request_id(),
            prompt_cache_key,
            tool_specs,
            system_prompt,
        ),
        events.clone(),
        Arc::clone(transport_stats),
    )
}

fn tool_runtime(workspace: &str, config: &ModelConfig, tools: &Tools) -> ToolRuntime {
    ToolRuntime::new_with_tools(
        workspace,
        tools.web_search_enabled().then(|| WebSearchConfig {
            endpoint: config.search_endpoint(),
            auth: config.auth.clone(),
        }),
        tools
            .image_generation_enabled()
            .then(|| ImageGenerationConfig {
                api_base_url: config.api_base_url.clone(),
                auth: config.auth.clone(),
                save_root: Path::new(workspace).to_path_buf(),
            }),
        tools,
    )
}

const fn status(success: bool) -> &'static str {
    if success { "completed" } else { "failed" }
}

const fn otel_status(success: bool) -> &'static str {
    if success { "OK" } else { "ERROR" }
}
