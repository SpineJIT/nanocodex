use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fmt,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use nanocodex_oai_api::{
    OpenAiAuth, ResponseItem, Tool, ToolContext, ToolDefinition, ToolExecution, ToolInput,
};
use schemars::{JsonSchema, r#gen::SchemaSettings};
use serde_json::value::to_raw_value;
use serde_json::{Map, Value, json};
use tracing::{Instrument, info, info_span};

#[cfg(feature = "code-mode")]
use crate::code_mode::{self, CodeModeExecution, CodeModeObserver};
use crate::{
    apply_patch, plan,
    shell::{self, ShellSessions},
    view_image,
};
#[cfg(feature = "remote-tools")]
use crate::{image_generation, web_search};

/// Owned context used when a Code Mode cell may outlive the model tool call
/// that started it.
#[doc(hidden)]
pub struct OwnedToolContext {
    pub(crate) model: String,
    pub(crate) session_id: String,
    pub(crate) call_id: String,
    pub(crate) history: Arc<Vec<ResponseItem>>,
    pub(crate) output_token_budget: usize,
}

impl OwnedToolContext {
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        session_id: impl Into<String>,
        call_id: impl Into<String>,
        history: Arc<Vec<ResponseItem>>,
        output_token_budget: usize,
    ) -> Self {
        Self {
            model: model.into(),
            session_id: session_id.into(),
            call_id: call_id.into(),
            history,
            output_token_budget,
        }
    }

    pub(crate) fn from_borrowed(context: ToolContext<'_>) -> Self {
        Self::new(
            context.model(),
            context.session_id(),
            context.call_id(),
            Arc::new(context.history().to_vec()),
            context.output_token_budget(),
        )
    }

    pub(crate) fn borrowed(&self) -> ToolContext<'_> {
        ToolContext::new(
            &self.model,
            &self.session_id,
            &self.call_id,
            self.history.as_slice(),
            self.output_token_budget,
        )
    }

    pub(crate) fn with_output_token_budget(mut self, output_token_budget: usize) -> Self {
        self.output_token_budget = output_token_budget;
        self
    }
}

/// A lazily populated family of Code Mode tools.
///
/// Providers start with the agent driver, advertise only their small direct
/// tool surface initially, and may make additional tools callable at runtime.
#[async_trait]
pub trait DynamicToolProvider: Send + Sync {
    /// Starts background discovery or connection work. Implementations must be idempotent.
    fn start(&self);

    /// Returns the provider's always-visible tools, such as `tool_search`.
    fn direct_tools(&self) -> Vec<Arc<dyn Tool>>;

    /// Returns deferred tools currently activated for new Code Mode cells.
    fn available_definitions(&self) -> Vec<ToolDefinition>;

    /// Executes an activated deferred tool, or returns `None` when this provider
    /// does not currently expose `name`.
    async fn execute(
        &self,
        name: &str,
        input: Value,
        context: ToolContext<'_>,
    ) -> Option<ToolExecution>;
}

/// Connection inputs for the built-in `OpenAI` web-search tool.
pub struct WebSearchConfig {
    /// Complete search endpoint URL.
    pub endpoint: String,
    /// Authentication source shared with the `OpenAI` API.
    pub auth: OpenAiAuth,
}

/// Connection and persistence inputs for the built-in image-generation tool.
pub struct ImageGenerationConfig {
    /// Base `OpenAI` API URL; image endpoint paths are appended internally.
    pub api_base_url: String,
    /// Authentication source shared with the `OpenAI` API.
    pub auth: OpenAiAuth,
    /// Root under which generated images are retained by session and call.
    pub save_root: PathBuf,
}

/// Declarative selection of the built-in tools installed for an agent.
#[derive(Clone)]
pub struct Tools {
    workspace: bool,
    web_search: bool,
    image_generation: bool,
    working_directory: Option<Arc<str>>,
    default_shell: Option<Arc<str>>,
    process_environment: Arc<Vec<(OsString, OsString)>>,
    #[cfg(feature = "remote-tools")]
    remote_http_client: Option<reqwest::Client>,
    registered: Vec<Arc<dyn Tool>>,
    providers: Vec<Arc<dyn DynamicToolProvider>>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            workspace: true,
            web_search: true,
            image_generation: true,
            working_directory: None,
            default_shell: None,
            process_environment: Arc::new(Vec::new()),
            #[cfg(feature = "remote-tools")]
            remote_http_client: None,
            registered: Vec::new(),
            providers: Vec::new(),
        }
    }
}

impl fmt::Debug for Tools {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(feature = "remote-tools")]
        let remote_http_client_configured = self.remote_http_client.is_some();
        #[cfg(not(feature = "remote-tools"))]
        let remote_http_client_configured = false;
        formatter
            .debug_struct("Tools")
            .field("workspace", &self.workspace)
            .field("web_search", &self.web_search)
            .field("image_generation", &self.image_generation)
            .field("working_directory", &self.working_directory)
            .field("default_shell", &self.default_shell)
            .field("process_environment_count", &self.process_environment.len())
            .field(
                "remote_http_client_configured",
                &remote_http_client_configured,
            )
            .field(
                "registered",
                &self
                    .registered
                    .iter()
                    .map(|tool| tool.definition().name().to_owned())
                    .collect::<Vec<_>>(),
            )
            .field("provider_count", &self.providers.len())
            .finish()
    }
}

impl Tools {
    /// Starts a builder with all standard tools enabled.
    #[must_use]
    pub fn builder() -> ToolsBuilder {
        ToolsBuilder::default()
    }

    /// Resumes configuring this tool selection while preserving its built-ins,
    /// registered tools, and dynamic providers.
    #[must_use]
    pub fn into_builder(self) -> ToolsBuilder {
        ToolsBuilder { tools: self }
    }

    /// Returns whether the standard workspace tools are enabled.
    #[must_use]
    pub const fn workspace_enabled(&self) -> bool {
        self.workspace
    }

    /// Returns whether the standard web-search tool is enabled.
    #[must_use]
    pub const fn web_search_enabled(&self) -> bool {
        self.web_search
    }

    /// Returns whether the standard image-generation tool is enabled.
    #[must_use]
    pub const fn image_generation_enabled(&self) -> bool {
        self.image_generation
    }

    fn process_environment(&self) -> Arc<Vec<(OsString, OsString)>> {
        Arc::clone(&self.process_environment)
    }

    #[cfg(feature = "remote-tools")]
    fn remote_http_client(&self) -> Option<reqwest::Client> {
        self.remote_http_client.clone()
    }

    /// Starts all dynamic providers without waiting for their handshakes.
    pub fn start_providers(&self) {
        for provider in &self.providers {
            provider.start();
        }
    }
}

/// Builder for the built-in tool selection.
#[derive(Default)]
pub struct ToolsBuilder {
    tools: Tools,
}

/// Invalid declarative tool selection.
#[derive(Debug, thiserror::Error)]
pub enum ToolsBuildError {
    /// A custom definition has an empty registry name.
    #[error("tool name must not be empty")]
    EmptyName,

    /// The model-visible working-directory override is empty.
    #[error("working directory override must not be empty")]
    EmptyWorkingDirectory,

    /// The model-visible shell override is empty.
    #[error("default shell override must not be empty")]
    EmptyDefaultShell,

    /// Two custom tools use the same definition name.
    #[error("tool name `{0}` is registered more than once")]
    DuplicateName(Box<str>),

    /// A custom tool collides with an enabled built-in tool.
    #[error("tool name `{0}` conflicts with an enabled built-in tool")]
    BuiltInName(Box<str>),
}

impl ToolsBuilder {
    /// Starts from an empty built-in tool set.
    #[must_use]
    pub fn without_defaults(mut self) -> Self {
        self.tools.workspace = false;
        self.tools.web_search = false;
        self.tools.image_generation = false;
        self
    }

    /// Enables or disables the standard command, patch, plan, and file tools.
    #[must_use]
    pub fn workspace(mut self, enabled: bool) -> Self {
        self.tools.workspace = enabled;
        self
    }

    /// Enables or disables the built-in direct web-search tool.
    #[must_use]
    pub fn web_search(mut self, enabled: bool) -> Self {
        self.tools.web_search = enabled;
        self
    }

    /// Enables or disables the built-in image-generation tool.
    #[must_use]
    pub fn image_generation(mut self, enabled: bool) -> Self {
        self.tools.image_generation = enabled;
        self
    }

    /// Overrides the default working directory described to the model.
    #[must_use]
    pub fn working_directory(mut self, directory: impl Into<Arc<str>>) -> Self {
        self.tools.working_directory = Some(directory.into());
        self
    }

    /// Overrides the default shell described to the model.
    #[must_use]
    pub fn default_shell(mut self, shell: impl Into<Arc<str>>) -> Self {
        self.tools.default_shell = Some(shell.into());
        self
    }

    /// Adds explicit environment overrides to workspace-tool child processes.
    ///
    /// Overrides are scoped to commands spawned by this tool selection and do
    /// not mutate the embedding process. A later value for the same name wins.
    #[must_use]
    pub fn process_environment<I, K, V>(mut self, variables: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let environment = Arc::make_mut(&mut self.tools.process_environment);
        for (name, value) in variables {
            let name = name.into();
            environment.retain(|(candidate, _)| candidate != &name);
            environment.push((name, value.into()));
        }
        self
    }

    /// Overrides the HTTP client used by in-process remote tools.
    #[cfg(feature = "remote-tools")]
    #[must_use]
    pub fn remote_http_client(mut self, client: reqwest::Client) -> Self {
        self.tools.remote_http_client = Some(client);
        self
    }

    /// Adds a function or freeform tool to the runtime.
    #[must_use]
    pub fn tool<T: Tool + 'static>(mut self, tool: T) -> Self {
        self.tools.registered.push(Arc::new(tool));
        self
    }

    /// Adds a dynamic family of Code Mode tools.
    #[must_use]
    pub fn provider<P: DynamicToolProvider + 'static>(mut self, provider: P) -> Self {
        let provider: Arc<dyn DynamicToolProvider> = Arc::new(provider);
        self.tools.registered.extend(provider.direct_tools());
        self.tools.providers.push(provider);
        self
    }

    /// Validates tool names and finishes the runtime configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, duplicate, or enabled built-in tool names.
    pub fn build(self) -> Result<Tools, ToolsBuildError> {
        if self
            .tools
            .working_directory
            .as_deref()
            .is_some_and(|directory| directory.trim().is_empty())
        {
            return Err(ToolsBuildError::EmptyWorkingDirectory);
        }
        if self
            .tools
            .default_shell
            .as_deref()
            .is_some_and(|shell| shell.trim().is_empty())
        {
            return Err(ToolsBuildError::EmptyDefaultShell);
        }
        let mut names = HashSet::with_capacity(self.tools.registered.len());
        for tool in &self.tools.registered {
            let definition = tool.definition();
            let name = definition.name();
            if name.is_empty() {
                return Err(ToolsBuildError::EmptyName);
            }
            if built_in_name(&self.tools, name) {
                return Err(ToolsBuildError::BuiltInName(name.into()));
            }
            if !names.insert(name.to_owned()) {
                return Err(ToolsBuildError::DuplicateName(name.into()));
            }
        }
        Ok(self.tools)
    }
}

fn built_in_name(tools: &Tools, name: &str) -> bool {
    (tools.workspace
        && matches!(
            name,
            "exec_command" | "write_stdin" | "update_plan" | "apply_patch" | "view_image"
        ))
        || (tools.web_search && name == "web__run")
        || (tools.image_generation && name == "image_gen__imagegen")
}

/// Stateful execution runtime for one agent driver.
///
/// A runtime retains Code Mode cells and shell sessions across calls. It is
/// normally owned privately by the higher-level agent driver.
pub struct ToolRuntime {
    registry: Arc<ToolRegistry>,
    #[cfg(feature = "code-mode")]
    code_mode: code_mode::CodeModeRuntime,
    sessions: Arc<ShellSessions>,
    default_shell_name: Arc<str>,
    working_directory: Arc<str>,
}

#[doc(hidden)]
#[derive(Clone)]
pub struct ToolRuntimeControl {
    #[cfg(feature = "code-mode")]
    code_mode: code_mode::CodeModeControl,
    sessions: Arc<ShellSessions>,
}

impl ToolRuntime {
    /// Creates a runtime with the standard workspace tools enabled.
    ///
    /// Pass `None` for either remote-tool configuration to omit that handler.
    pub fn new(
        workspace: impl Into<PathBuf>,
        web_search: Option<WebSearchConfig>,
        image_generation: Option<ImageGenerationConfig>,
    ) -> Self {
        Self::new_inner(
            workspace,
            web_search,
            image_generation,
            true,
            Arc::new(Vec::new()),
            #[cfg(feature = "remote-tools")]
            None,
        )
    }

    /// Builds the runtime from one complete declarative tool selection.
    #[must_use]
    pub fn new_with_tools(
        workspace: impl Into<PathBuf>,
        web_search: Option<WebSearchConfig>,
        image_generation: Option<ImageGenerationConfig>,
        tools: &Tools,
    ) -> Self {
        Self::new_inner(
            workspace,
            web_search,
            image_generation,
            tools.workspace_enabled(),
            tools.process_environment(),
            #[cfg(feature = "remote-tools")]
            tools.remote_http_client(),
        )
        .with_tools(tools)
    }

    fn new_inner(
        workspace: impl Into<PathBuf>,
        web_search: Option<WebSearchConfig>,
        image_generation: Option<ImageGenerationConfig>,
        workspace_enabled: bool,
        process_environment: Arc<Vec<(OsString, OsString)>>,
        #[cfg(feature = "remote-tools")] remote_http_client: Option<reqwest::Client>,
    ) -> Self {
        let workspace = workspace.into();
        let sessions = Arc::new(ShellSessions::with_environment(process_environment));
        let default_shell_name = Arc::from(sessions.default_shell_name());
        let working_directory = Arc::from(workspace.to_string_lossy().into_owned());
        #[cfg(feature = "code-mode")]
        let code_mode_workspace = workspace.clone();
        let mut handlers: Vec<Arc<dyn Tool>> = Vec::new();
        if workspace_enabled {
            handlers.extend([
                Arc::new(apply_patch::ApplyPatchHandler::new(workspace.clone())) as Arc<dyn Tool>,
                Arc::new(shell::ExecCommandHandler::new(
                    workspace.clone(),
                    Arc::clone(&sessions),
                )),
                Arc::new(plan::UpdatePlanTool::new()),
                Arc::new(view_image::ViewImageHandler::new(workspace.clone())),
                Arc::new(shell::WriteStdinHandler::new(Arc::clone(&sessions))),
            ]);
        }
        #[cfg(feature = "remote-tools")]
        {
            let remote_http_client = remote_http_client.unwrap_or_default();
            if let Some(web_search) = web_search {
                handlers.push(Arc::new(web_search::WebSearchHandler::with_client(
                    web_search,
                    remote_http_client.clone(),
                )));
            }
            if let Some(image_generation) = image_generation {
                handlers.push(Arc::new(
                    image_generation::ImageGenerationHandler::with_client(
                        image_generation,
                        remote_http_client,
                    ),
                ));
            }
        }
        #[cfg(not(feature = "remote-tools"))]
        let _ = (web_search, image_generation);
        Self {
            registry: Arc::new(ToolRegistry::from_ordered(handlers)),
            #[cfg(feature = "code-mode")]
            code_mode: code_mode::CodeModeRuntime::new(code_mode_workspace),
            sessions,
            default_shell_name,
            working_directory,
        }
    }

    /// Extends this runtime with a validated declarative tool selection.
    #[must_use]
    pub fn with_tools(mut self, tools: &Tools) -> Self {
        let registry = Arc::make_mut(&mut self.registry);
        registry.extend(tools.registered.iter().cloned());
        registry.providers.extend(tools.providers.iter().cloned());
        if let Some(working_directory) = &tools.working_directory {
            self.working_directory = Arc::clone(working_directory);
        }
        if let Some(default_shell) = &tools.default_shell {
            self.default_shell_name = Arc::clone(default_shell);
        }
        self
    }

    /// Returns the shell name described to the model.
    #[must_use]
    pub fn default_shell_name(&self) -> &str {
        &self.default_shell_name
    }

    /// Returns the working directory described to the model.
    #[must_use]
    pub fn working_directory(&self) -> &str {
        &self.working_directory
    }

    #[doc(hidden)]
    #[must_use]
    pub fn control(&self) -> ToolRuntimeControl {
        ToolRuntimeControl {
            #[cfg(feature = "code-mode")]
            code_mode: self.code_mode.control(),
            sessions: Arc::clone(&self.sessions),
        }
    }

    #[cfg(feature = "code-mode")]
    /// Returns the direct model-visible Code Mode tool definitions.
    #[must_use]
    pub fn model_specs(&self) -> Vec<ToolDefinition> {
        vec![
            code_mode::exec_spec(
                self.registry.definitions(),
                !self.registry.providers.is_empty(),
            ),
            code_mode::wait_spec(),
        ]
    }

    #[cfg(feature = "code-mode")]
    /// Starts or resumes a Code Mode cell and observes its first terminal boundary.
    pub async fn execute_code(&self, source: &str, context: ToolContext<'_>) -> CodeModeExecution {
        self.code_mode
            .execute(
                source,
                Arc::clone(&self.registry),
                OwnedToolContext::from_borrowed(context),
            )
            .await
    }

    #[cfg(feature = "code-mode")]
    #[doc(hidden)]
    pub async fn execute_code_with_updates(
        &self,
        source: &str,
        context: ToolContext<'_>,
        observer: &mut dyn CodeModeObserver,
    ) -> CodeModeExecution {
        self.code_mode
            .execute_with_updates(
                source,
                Arc::clone(&self.registry),
                OwnedToolContext::from_borrowed(context),
                observer,
            )
            .await
    }

    #[cfg(feature = "code-mode")]
    /// Executes Code Mode without copying an already-owned history snapshot.
    #[doc(hidden)]
    pub async fn execute_code_owned(
        &self,
        source: &str,
        context: OwnedToolContext,
    ) -> CodeModeExecution {
        self.code_mode
            .execute(source, Arc::clone(&self.registry), context)
            .await
    }

    #[cfg(feature = "code-mode")]
    #[doc(hidden)]
    pub async fn execute_code_owned_with_updates(
        &self,
        source: &str,
        context: OwnedToolContext,
        observer: &mut dyn CodeModeObserver,
    ) -> CodeModeExecution {
        self.code_mode
            .execute_with_updates(source, Arc::clone(&self.registry), context, observer)
            .await
    }

    #[cfg(feature = "code-mode")]
    /// Waits for a previously yielded Code Mode cell.
    pub async fn wait_for_code(&self, input: &str, context: ToolContext<'_>) -> CodeModeExecution {
        self.code_mode.wait(input, context).await
    }

    #[cfg(feature = "code-mode")]
    #[doc(hidden)]
    pub async fn wait_for_code_with_updates(
        &self,
        input: &str,
        _context: ToolContext<'_>,
        observer: &mut dyn CodeModeObserver,
    ) -> CodeModeExecution {
        self.code_mode.wait_with_updates(input, observer).await
    }

    /// Executes one registered tool through this runtime's retained state.
    ///
    /// Shell sessions created by `exec_command` remain available to later
    /// `write_stdin` calls on the same runtime.
    pub async fn execute_tool(
        &self,
        name: &str,
        input: ToolInput,
        context: ToolContext<'_>,
    ) -> ToolExecution {
        self.registry.execute_direct(name, input, context).await
    }
}

impl ToolRuntimeControl {
    #[doc(hidden)]
    pub async fn cancel(&self) {
        #[cfg(feature = "code-mode")]
        tokio::join!(
            self.code_mode.terminate_all(),
            self.sessions.terminate_all()
        );
        #[cfg(not(feature = "code-mode"))]
        self.sessions.terminate_all().await;
    }
}

#[derive(Clone)]
pub(crate) struct ToolRegistry {
    ordered: Vec<Arc<dyn Tool>>,
    definitions: Vec<ToolDefinition>,
    by_name: HashMap<Box<str>, usize>,
    providers: Vec<Arc<dyn DynamicToolProvider>>,
}

impl ToolRegistry {
    async fn execute_direct(
        &self,
        name: &str,
        input: ToolInput,
        context: ToolContext<'_>,
    ) -> ToolExecution {
        let Some((handler, _definition)) = self.get(name) else {
            return ToolExecution::error(format!("unsupported tool call: {name}"));
        };
        match handler.execute(input, context).await {
            Ok(execution) => execution,
            Err(error) => ToolExecution::error(error.to_string()),
        }
    }

    pub(crate) async fn execute_nested(
        &self,
        name: &str,
        input: Value,
        context: ToolContext<'_>,
    ) -> ToolExecution {
        let trace_content = tracing::enabled!(
            target: "nanocodex_tools",
            tracing::Level::INFO
        );
        let arguments_content = trace_content
            .then(|| serde_json::to_string(&input).ok())
            .flatten();
        let arguments_bytes = arguments_content.as_ref().map_or(0, String::len);
        let arguments_kind = match &input {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        let arguments_count = input.as_object().map_or_else(
            || input.as_array().map_or(1, Vec::len),
            serde_json::Map::len,
        );
        let argument_keys = trace_content
            .then(|| {
                input.as_object().map(|object| {
                    object
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
            })
            .flatten()
            .unwrap_or_default();
        let span = info_span!(
            target: "nanocodex_tools",
            "tool.execute",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            tool.name = name,
            session.id = context.session_id(),
            tool.call_id = context.call_id(),
            tool.arguments.bytes = arguments_bytes,
            tool.arguments.kind = arguments_kind,
            tool.arguments.count = arguments_count,
            tool.arguments.keys = argument_keys,
            process.exit.code = tracing::field::Empty,
            process.running = tracing::field::Empty,
            process.wall_time_ms = tracing::field::Empty,
            shell.session.id = tracing::field::Empty,
            tool.output.bytes = tracing::field::Empty,
            tool.output.original_tokens = tracing::field::Empty,
            status = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        if let Some(arguments_content) = &arguments_content {
            record_tool_content(&span, "tool.arguments", arguments_content);
        }
        let started_at = std::time::Instant::now();
        let execution = self
            .execute_nested_inner(name, input, context)
            .instrument(span.clone())
            .await;
        if trace_content && let Ok(content) = serde_json::to_string(&execution.output) {
            record_tool_content(&span, "tool.output", &content);
        }
        span.record(
            "status",
            if execution.success {
                "completed"
            } else {
                "failed"
            },
        );
        span.record(
            "otel.status_code",
            if execution.success { "OK" } else { "ERROR" },
        );
        span.record(
            "duration_ns",
            u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
        if let Some(process) = execution.process_trace() {
            if let Some(exit_code) = process.exit_code {
                span.record("process.exit.code", exit_code);
            }
            span.record("process.running", process.session_id.is_some());
            span.record("process.wall_time_ms", process.wall_time_seconds * 1_000.0);
            if let Some(session_id) = process.session_id {
                span.record("shell.session.id", session_id);
            }
            span.record("tool.output.bytes", process.output_bytes);
            if let Some(original_token_count) = process.original_token_count {
                span.record("tool.output.original_tokens", original_token_count);
            }
        }
        execution
    }

    async fn execute_nested_inner(
        &self,
        name: &str,
        input: Value,
        context: ToolContext<'_>,
    ) -> ToolExecution {
        let Some((handler, definition)) = self.get(name) else {
            for provider in &self.providers {
                if let Some(execution) = provider.execute(name, input.clone(), context).await {
                    return execution;
                }
            }
            return ToolExecution::error(format!("unsupported nested tool call: {name}"));
        };
        let input = match definition {
            ToolDefinition::Function { .. } if !input.is_object() => {
                return ToolExecution::error(format!(
                    "nested function tool {name} requires an object argument"
                ));
            }
            ToolDefinition::Function { .. } => match to_raw_value(&input) {
                Ok(input) => ToolInput::Function(input),
                Err(error) => {
                    return ToolExecution::error(format!("failed to encode {name} input: {error}"));
                }
            },
            ToolDefinition::Custom { .. } => match input.as_str() {
                Some(input) => ToolInput::Freeform(input.to_owned()),
                None => {
                    return ToolExecution::error(format!(
                        "nested freeform tool {name} requires a string argument"
                    ));
                }
            },
        };
        match handler.execute(input, context).await {
            Ok(execution) => execution,
            Err(error) => ToolExecution::error(error.to_string()),
        }
    }

    pub(crate) fn nested_tool_metadata(&self) -> Vec<Value> {
        let mut metadata = self
            .entries()
            .map(|(_, definition)| definition_metadata(definition.name(), definition))
            .collect::<Vec<_>>();
        for definition in self
            .providers
            .iter()
            .flat_map(|provider| provider.available_definitions())
        {
            metadata.push(definition_metadata(definition.name(), &definition));
        }
        metadata
    }
    fn from_ordered(ordered: Vec<Arc<dyn Tool>>) -> Self {
        let definitions = ordered
            .iter()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        let by_name = definitions
            .iter()
            .enumerate()
            .map(|(index, definition)| (definition.name().into(), index))
            .collect();
        Self {
            ordered,
            definitions,
            by_name,
            providers: Vec::new(),
        }
    }

    fn extend(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        for tool in tools {
            let index = self.ordered.len();
            let definition = tool.definition();
            self.by_name.insert(definition.name().into(), index);
            self.definitions.push(definition);
            self.ordered.push(tool);
        }
    }

    fn get(&self, name: &str) -> Option<(&Arc<dyn Tool>, &ToolDefinition)> {
        let index = *self.by_name.get(name)?;
        Some((self.ordered.get(index)?, self.definitions.get(index)?))
    }

    pub(crate) fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    fn entries(&self) -> impl Iterator<Item = (&Arc<dyn Tool>, &ToolDefinition)> {
        self.ordered.iter().zip(&self.definitions)
    }
}

fn record_tool_content(span: &tracing::Span, kind: &'static str, content: &str) {
    span.in_scope(|| {
        info!(
            target: "nanocodex_tools",
            content_kind = kind,
            content,
            "tool content"
        );
    });
}

fn definition_metadata(name: &str, definition: &ToolDefinition) -> Value {
    let kind = match definition {
        ToolDefinition::Function { .. } => "function",
        ToolDefinition::Custom { .. } => "freeform",
    };
    #[cfg(feature = "code-mode")]
    let metadata_name = code_mode::description::normalize_identifier(name);
    #[cfg(not(feature = "code-mode"))]
    let metadata_name = name;
    json!({
        "name": metadata_name,
        "tool_name": name,
        "description": definition.description(),
        "kind": kind,
    })
}

/// Produces the compact JSON Schema shape used for macro-generated tools.
#[doc(hidden)]
#[must_use]
pub fn schema_for<T: JsonSchema>() -> Value {
    let schema = SchemaSettings::draft2019_09()
        .with(|settings| {
            settings.inline_subschemas = true;
            settings.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<T>();
    let Value::Object(mut schema) =
        serde_json::to_value(schema).expect("a schemars root schema should serialize to an object")
    else {
        unreachable!("a schemars root schema should be an object");
    };
    let mut tool_schema = Map::new();
    for key in [
        "properties",
        "required",
        "type",
        "additionalProperties",
        "$defs",
        "definitions",
        "enum",
        "const",
        "anyOf",
        "oneOf",
        "allOf",
    ] {
        if let Some(value) = schema.remove(key) {
            tool_schema.insert(key.to_owned(), value);
        }
    }
    Value::Object(tool_schema)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use nanocodex_oai_api::{OpenAiAuth, ToolDefinition};
    use serde::Deserialize;
    use serde_json::json;

    use crate::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolOutputBody, ToolResult};

    use super::{
        DynamicToolProvider, ImageGenerationConfig, Tool, ToolContext, ToolExecution, ToolInput,
        ToolRuntime, Tools, WebSearchConfig,
    };

    struct Double;

    struct Fails;

    struct ReplacementExec;

    struct Search {
        activated: Arc<AtomicBool>,
    }

    struct DeferredProvider {
        activated: Arc<AtomicBool>,
        started: AtomicBool,
    }

    #[derive(Deserialize)]
    struct DoubleInput {
        value: i64,
    }

    #[async_trait::async_trait]
    impl Tool for Double {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::function(
                "double",
                "Doubles an integer.",
                json!({
                    "type": "object",
                    "properties": { "value": { "type": "integer" } },
                    "required": ["value"],
                    "additionalProperties": false
                }),
            )
        }

        async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
            let input = input.decode_json::<DoubleInput>()?;
            Ok(ToolExecution::text((input.value * 2).to_string()))
        }
    }

    #[async_trait::async_trait]
    impl Tool for Fails {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::function(
                "fails",
                "Always fails.",
                json!({ "type": "object", "properties": {} }),
            )
        }

        async fn execute(&self, _input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
            Err(std::io::Error::other("intentional handler failure").into())
        }
    }

    #[async_trait::async_trait]
    impl Tool for ReplacementExec {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::function(
                "exec_command",
                "Replacement command executor.",
                json!({
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } },
                    "required": ["cmd"],
                    "additionalProperties": false
                }),
            )
        }

        async fn execute(&self, _input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
            Ok(ToolExecution::text("replacement"))
        }
    }

    #[async_trait::async_trait]
    impl Tool for Search {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::function(
                "tool_search",
                "Activates a matching deferred tool.",
                json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            )
        }

        async fn execute(&self, _input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
            self.activated.store(true, Ordering::Release);
            Ok(ToolExecution::from_json(
                json!({ "name": "deferred_echo" }),
                true,
            ))
        }
    }

    #[async_trait::async_trait]
    impl DynamicToolProvider for DeferredProvider {
        fn start(&self) {
            self.started.store(true, Ordering::Release);
        }

        fn direct_tools(&self) -> Vec<Arc<dyn Tool>> {
            vec![Arc::new(Search {
                activated: Arc::clone(&self.activated),
            })]
        }

        fn available_definitions(&self) -> Vec<ToolDefinition> {
            self.activated
                .load(Ordering::Acquire)
                .then(|| {
                    ToolDefinition::function(
                        "deferred_echo",
                        "Returns its input.",
                        json!({ "type": "object", "properties": {} }),
                    )
                })
                .into_iter()
                .collect()
        }

        async fn execute(
            &self,
            name: &str,
            input: serde_json::Value,
            _context: ToolContext<'_>,
        ) -> Option<ToolExecution> {
            (name == "deferred_echo" && self.activated.load(Ordering::Acquire))
                .then(|| ToolExecution::from_json(input, true))
        }
    }

    fn runtime(web_search: bool) -> ToolRuntime {
        ToolRuntime::new(
            ".",
            web_search.then(|| WebSearchConfig {
                endpoint: "http://127.0.0.1:1/v1/alpha/search".to_owned(),
                auth: OpenAiAuth::api_key("test-key"),
            }),
            Some(ImageGenerationConfig {
                api_base_url: "http://127.0.0.1:1/v1".to_owned(),
                auth: OpenAiAuth::api_key("test-key"),
                save_root: std::env::temp_dir().join("nanocodex-test-images"),
            }),
        )
    }

    #[test]
    fn web_search_handler_and_spec_are_absent_when_disabled() {
        let enabled = runtime(true);
        assert!(
            enabled
                .registry
                .entries()
                .any(|(_, definition)| definition.name() == "web__run")
        );
        let enabled_specs = serde_json::to_value(enabled.model_specs()).unwrap();
        assert!(
            enabled_specs[0]["description"]
                .as_str()
                .is_some_and(|description| description.contains("`web__run`"))
        );

        let disabled = runtime(false);
        assert!(
            disabled
                .registry
                .entries()
                .all(|(_, definition)| definition.name() != "web__run")
        );
        let disabled_specs = serde_json::to_value(disabled.model_specs()).unwrap();
        assert!(
            disabled_specs[0]["description"]
                .as_str()
                .is_some_and(|description| !description.contains("`web__run`"))
        );
    }

    #[test]
    fn without_defaults_allows_replacing_a_standard_workspace_tool() {
        assert!(Tools::builder().tool(ReplacementExec).build().is_err());

        let tools = Tools::builder()
            .without_defaults()
            .tool(ReplacementExec)
            .build()
            .unwrap();
        let runtime = ToolRuntime::new_with_tools(".", None, None, &tools);
        let names = runtime
            .registry
            .entries()
            .map(|(_, definition)| definition.name())
            .collect::<Vec<_>>();

        assert_eq!(names, ["exec_command"]);
    }

    #[test]
    fn tool_recipe_overrides_model_visible_environment_context() {
        let tools = Tools::builder()
            .without_defaults()
            .working_directory("/workspace")
            .default_shell("sh")
            .build()
            .unwrap();
        let runtime = ToolRuntime::new_with_tools("/host/attempt", None, None, &tools);

        assert_eq!(runtime.working_directory(), "/workspace");
        assert_eq!(runtime.default_shell_name(), "sh");
    }

    #[test]
    fn tool_recipe_rejects_empty_environment_overrides() {
        assert!(matches!(
            Tools::builder().working_directory(" ").build(),
            Err(super::ToolsBuildError::EmptyWorkingDirectory)
        ));
        assert!(matches!(
            Tools::builder().default_shell("").build(),
            Err(super::ToolsBuildError::EmptyDefaultShell)
        ));
    }

    #[tokio::test]
    async fn registered_tool_is_described_and_callable_from_code_mode() {
        let tools = Tools::builder()
            .without_defaults()
            .tool(Double)
            .build()
            .unwrap();
        let runtime = ToolRuntime::new(".", None, None).with_tools(&tools);
        let description = serde_json::to_value(runtime.model_specs()).unwrap();
        assert!(
            description[0]["description"]
                .as_str()
                .is_some_and(|description| description.contains(
                    "declare const tools: { double(args: { value: number; }): Promise<unknown>; };"
                ))
        );

        let execution = runtime
            .execute_code(
                r"
const result = await tools.double({ value: 21 });
text(result);
",
                ToolContext::new(
                    "test-model",
                    "test-session",
                    "test-call",
                    &[],
                    DEFAULT_TOOL_OUTPUT_TOKENS,
                ),
            )
            .await;
        assert!(execution.success);
        assert_eq!(execution.nested_calls.len(), 1);
        assert_eq!(execution.nested_calls[0].name, "double");
        assert_eq!(execution.nested_calls[0].input, json!({ "value": 21 }));
        let ToolOutputBody::Content(content) = execution.output else {
            panic!("expected content output");
        };
        assert_eq!(
            serde_json::to_value(content)
                .unwrap()
                .as_array()
                .unwrap()
                .last(),
            Some(&json!({ "type": "input_text", "text": "42" }))
        );
    }

    #[tokio::test]
    async fn handler_errors_become_failed_model_visible_results() {
        let tools = Tools::builder()
            .without_defaults()
            .tool(Fails)
            .build()
            .unwrap();
        let runtime = ToolRuntime::new(".", None, None).with_tools(&tools);
        let execution = runtime
            .registry
            .execute_nested(
                "fails",
                json!({}),
                ToolContext::new(
                    "test-model",
                    "test-session",
                    "test-call",
                    &[],
                    DEFAULT_TOOL_OUTPUT_TOKENS,
                ),
            )
            .await;

        assert!(!execution.success);
        assert!(matches!(
            execution.output,
            ToolOutputBody::Text(output) if output == "intentional handler failure"
        ));
    }

    #[tokio::test]
    async fn code_mode_can_search_and_call_a_deferred_tool_in_one_cell() {
        let tools = Tools::builder()
            .without_defaults()
            .provider(DeferredProvider {
                activated: Arc::new(AtomicBool::new(false)),
                started: AtomicBool::new(false),
            })
            .build()
            .unwrap();
        tools.start_providers();
        let runtime = ToolRuntime::new(".", None, None).with_tools(&tools);
        let model_specs_before = serde_json::to_vec(&runtime.model_specs()).unwrap();
        let execution = runtime
            .execute_code(
                r#"
const found = await tools.tool_search({ query: "echo" });
const result = await tools[found.name]({ value: 21 });
text(result.value);
"#,
                ToolContext::new(
                    "test-model",
                    "test-session",
                    "test-call",
                    &[],
                    DEFAULT_TOOL_OUTPUT_TOKENS,
                ),
            )
            .await;

        assert!(execution.success);
        assert_eq!(
            serde_json::to_vec(&runtime.model_specs()).unwrap(),
            model_specs_before,
            "activating deferred tools must not change the model request prefix"
        );
        assert_eq!(execution.nested_calls.len(), 2);
        assert_eq!(execution.nested_calls[0].name, "tool_search");
        assert_eq!(execution.nested_calls[1].name, "deferred_echo");
        let ToolOutputBody::Content(content) = execution.output else {
            panic!("expected content output");
        };
        assert_eq!(
            serde_json::to_value(content)
                .unwrap()
                .as_array()
                .unwrap()
                .last(),
            Some(&json!({ "type": "input_text", "text": "21" }))
        );
    }
}
