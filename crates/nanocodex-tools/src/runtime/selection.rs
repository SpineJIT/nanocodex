use super::*;

/// Nanocodex's model-visible tool exposure policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolExposure {
    /// Expose normal tools only through Code Mode's `exec` entrypoint.
    #[default]
    CodeModeOnly,
    /// Expose `exec` and `wait` before ordinary direct tools, while retaining
    /// the same handlers for calls composed through Code Mode.
    DirectAndCodeMode,
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

    /// Returns the provider's direct tools for one model exposure policy.
    ///
    /// Providers normally expose the same tools under either policy. A provider may
    /// hide discovery entrypoints in Code Mode-only when its deferred tools
    /// are already callable through `ALL_TOOLS`, matching Codex's MCP policy.
    fn direct_tools_for_exposure(&self, _exposure: ToolExposure) -> Vec<Arc<dyn Tool>> {
        self.direct_tools()
    }

    /// Returns deferred tools currently callable from new Code Mode cells.
    fn available_definitions(&self) -> Vec<ToolDefinition>;

    /// Returns whether this provider currently exposes `name`.
    fn contains(&self, name: &str) -> bool {
        self.available_definitions()
            .iter()
            .any(|definition| definition.name() == name)
    }

    /// Returns whether a callable deferred tool is safe to execute in parallel.
    ///
    /// Providers are conservative by default. Implementations must return
    /// `true` only for a currently callable tool with explicit safety
    /// metadata.
    fn supports_parallel_tool_calls(&self, _name: &str) -> bool {
        false
    }

    /// Executes a callable deferred tool, or returns `None` when this provider
    /// does not currently expose `name`.
    ///
    /// The owning runtime converts handler panics into a failed `aborted`
    /// output; they never unwind through the runtime owner.
    async fn execute(
        &self,
        name: &str,
        input: Value,
        context: ToolContext<'_>,
    ) -> Option<ToolOutput>;
}

/// Declarative selection of the built-in tools installed for an agent.
#[derive(Clone)]
pub struct Tools {
    exposure: ToolExposure,
    workspace: bool,
    web_search: bool,
    image_generation: bool,
    pub(super) working_directory: Option<Arc<str>>,
    pub(super) default_shell: Option<Arc<str>>,
    process_environment: Arc<Vec<(OsString, OsString)>>,
    remote_http_client: Option<reqwest::Client>,
    pub(super) registered: Vec<Arc<dyn Tool>>,
    pub(super) provider_direct: Vec<Arc<dyn Tool>>,
    pub(super) providers: Vec<Arc<dyn DynamicToolProvider>>,
    pub(super) deferred_tools_guidance_enabled: bool,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            exposure: ToolExposure::default(),
            workspace: true,
            web_search: true,
            image_generation: true,
            working_directory: None,
            default_shell: None,
            process_environment: Arc::new(Vec::new()),
            remote_http_client: None,
            registered: Vec::new(),
            provider_direct: Vec::new(),
            providers: Vec::new(),
            deferred_tools_guidance_enabled: false,
        }
    }
}

impl fmt::Debug for Tools {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let remote_http_client_configured = self.remote_http_client.is_some();
        formatter
            .debug_struct("Tools")
            .field("exposure", &self.exposure)
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
            .field(
                "provider_direct",
                &self
                    .provider_direct
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
    pub const fn into_builder(self) -> ToolsBuilder {
        ToolsBuilder { tools: self }
    }

    /// Returns the model-visible tool exposure policy.
    #[must_use]
    pub const fn exposure(&self) -> ToolExposure {
        self.exposure
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

    /// Returns this tool selection bound to one agent session.
    ///
    /// Native workspace commands receive the session ID through
    /// `CODEX_THREAD_ID`. This binding replaces a caller-provided value without
    /// mutating other clones of the tool selection.
    #[must_use]
    pub fn for_session(mut self, session_id: &str) -> Self {
        self.insert_process_environment(CODEX_THREAD_ID_ENV_VAR.into(), session_id.into());
        self
    }

    pub(super) fn process_environment(&self) -> Arc<Vec<(OsString, OsString)>> {
        Arc::clone(&self.process_environment)
    }

    fn insert_process_environment(&mut self, name: OsString, value: OsString) {
        let environment = Arc::make_mut(&mut self.process_environment);
        environment.retain(|(candidate, _)| candidate != &name);
        environment.push((name, value));
    }

    pub(super) fn remote_http_client(&self) -> Option<reqwest::Client> {
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
    /// Selects whether registered tools are also exposed directly to the model.
    ///
    /// The default is [`ToolExposure::CodeModeOnly`]. This changes only the
    /// model-visible declaration set; all registered handlers remain callable
    /// from Code Mode.
    #[must_use]
    pub const fn exposure(mut self, exposure: ToolExposure) -> Self {
        self.tools.exposure = exposure;
        self
    }

    /// Starts from an empty built-in tool set.
    #[must_use]
    pub const fn without_defaults(mut self) -> Self {
        self.tools.workspace = false;
        self.tools.web_search = false;
        self.tools.image_generation = false;
        self
    }

    /// Enables or disables the standard command, patch, plan, and file tools.
    #[must_use]
    pub const fn workspace(mut self, enabled: bool) -> Self {
        self.tools.workspace = enabled;
        self
    }

    /// Enables or disables the built-in direct web-search tool.
    #[must_use]
    pub const fn web_search(mut self, enabled: bool) -> Self {
        self.tools.web_search = enabled;
        self
    }

    /// Enables or disables the built-in image-generation tool.
    #[must_use]
    pub const fn image_generation(mut self, enabled: bool) -> Self {
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
        for (name, value) in variables {
            self.tools
                .insert_process_environment(name.into(), value.into());
        }
        self
    }

    /// Overrides the HTTP client used by in-process remote tools.
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
        self.tools.providers.push(provider);
        self.refresh_provider_direct();
        self
    }

    /// Validates tool names and finishes the runtime configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, duplicate, or enabled built-in tool names.
    pub fn build(mut self) -> Result<Tools, ToolsBuildError> {
        self.refresh_provider_direct();
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
        let mut names = HashSet::with_capacity(
            self.tools
                .registered
                .len()
                .saturating_add(self.tools.provider_direct.len()),
        );
        for tool in self
            .tools
            .registered
            .iter()
            .chain(&self.tools.provider_direct)
        {
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

    fn refresh_provider_direct(&mut self) {
        self.tools.deferred_tools_guidance_enabled = self.tools.providers.iter().any(|provider| {
            provider
                .direct_tools()
                .iter()
                .any(|tool| matches!(tool.definition(), ToolDefinition::ToolSearch { .. }))
        });
        self.tools.provider_direct = self
            .tools
            .providers
            .iter()
            .flat_map(|provider| provider.direct_tools_for_exposure(self.tools.exposure))
            .collect();
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
