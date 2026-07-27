use super::*;

#[cfg(not(target_family = "wasm"))]
use crate::rollout::RolloutConfig;

/// Builder for one owned agent lifecycle.
#[derive(Clone)]
pub struct NanocodexBuilder<F = StandardServiceFactory> {
    pub(super) config: ModelConfig,
    pub(super) tools: ToolsConfiguration,
    pub(super) workspace: Option<PathBuf>,
    pub(super) session_id: Option<SessionId>,
    pub(super) prompt_cache: PromptCacheConfig,
    pub(super) codex: CodexCompatibility,
    pub(super) resume: Option<SessionSnapshot>,
    pub(super) factory: F,
}

#[derive(Clone, Default)]
pub(super) struct PromptCacheConfig {
    pub(super) key: Option<String>,
    pub(super) shared: Option<SharedPromptCache>,
}

#[derive(Clone, Default)]
pub(super) struct CodexCompatibility {
    pub(super) context: ContextSourceConfig,
    pub(super) durability: DurabilityConfig,
}

impl<F> NanocodexBuilder<F> {
    /// Replaces the stable system/developer instructions.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<Arc<str>>) -> Self {
        self.config.system_prompt = instructions.into();
        self
    }

    /// Sets the model thinking level. The default is [`Thinking::High`].
    #[must_use]
    pub const fn thinking(mut self, thinking: Thinking) -> Self {
        self.config.thinking = thinking;
        self
    }

    /// Enables priority processing. The default is disabled.
    #[must_use]
    pub const fn fast_mode(mut self, enabled: bool) -> Self {
        self.config.fast_mode = enabled;
        self
    }

    /// Sets the Responses reasoning execution mode. The default is
    /// [`ReasoningMode::Standard`].
    #[must_use]
    pub const fn reasoning_mode(mut self, reasoning_mode: ReasoningMode) -> Self {
        self.config.reasoning_mode = reasoning_mode;
        self
    }

    /// Replaces the standard built-in tool selection.
    #[must_use]
    pub fn tools(mut self, tools: Tools) -> Self {
        self.tools = ToolsConfiguration::Shared(tools);
        self
    }

    /// Builds a fresh tool collection for every agent driver.
    ///
    /// The factory receives a weak capability targeting the driver whose tool
    /// runtime is being built. Use this for agent-relative tools such as Code
    /// Mode child-agent tools; stateless tools may continue using
    /// [`Self::tools`].
    #[cfg(not(target_family = "wasm"))]
    #[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
    #[must_use]
    pub fn tools_factory<T>(mut self, factory: T) -> Self
    where
        T: Fn(AgentHandle) -> std::result::Result<Tools, ToolsBuildError> + Send + Sync + 'static,
    {
        self.tools = ToolsConfiguration::PerAgent(Arc::new(factory));
        self
    }

    /// Fixes the workspace used by every prompt in this agent session.
    #[must_use]
    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Sets the root agent's `UUIDv7` session identity.
    ///
    /// The root identity also seeds its checkpoint lineage. Spawned siblings
    /// and forks receive fresh session IDs; forks retain the root's opaque
    /// lineage so [`Nanocodex::fork_from`] can reject unrelated results.
    #[must_use]
    pub const fn session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// Sets a stable cache identity for the immutable request prefix.
    ///
    /// Independent root agents may share this key without sharing their
    /// session, conversation, response chain, tools, or workspace. When
    /// omitted, each independently built root uses its own session lineage.
    /// Clean children and forks inherit their root's cache identity.
    #[must_use]
    pub fn prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache.key = Some(prompt_cache_key.into());
        self
    }

    /// Shares completed immutable-prefix warmups among builders cloned from
    /// this recipe.
    ///
    /// The first agent primes the provider cache. Other agents skip the
    /// redundant warmup and send their first complete generation with the same
    /// prefix cache key. Every clean agent still owns an independent session,
    /// conversation, response chain, service stack, tool runtime, event stream,
    /// and workspace. Entries are fingerprinted from the exact prefix and key.
    #[must_use]
    pub fn shared_prompt_cache(mut self) -> Self {
        self.prompt_cache.shared = Some(SharedPromptCache::default());
        self
    }

    /// Loads global user instructions from `AGENTS.override.md` or `AGENTS.md`
    /// in the supplied Codex state directory.
    #[cfg(not(target_family = "wasm"))]
    #[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
    #[must_use]
    pub fn codex_home(mut self, codex_home: impl Into<PathBuf>) -> Self {
        self.codex.context.set_codex_home(codex_home.into());
        self
    }

    /// Records committed history in Codex's resumable JSONL rollout layout.
    #[cfg(not(target_family = "wasm"))]
    #[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
    #[must_use]
    pub fn rollout(mut self, rollout: RolloutConfig) -> Self {
        if self.codex.context.codex_home().is_none() {
            self.codex
                .context
                .set_codex_home(rollout.codex_home().to_path_buf());
        }
        self.codex.durability.set_rollout(rollout);
        self
    }

    /// Restores a completed session boundary into a fresh driver, WebSocket,
    /// and tool runtime while retaining its typed history and cache lineage.
    ///
    /// An explicitly configured session ID names the new runtime/event stream;
    /// it does not replace the snapshot's prompt-cache lineage. Configure the
    /// same instructions, tool definitions, and custom handlers used by the
    /// original session; incompatible policy is rejected during [`Self::build`].
    #[must_use]
    pub fn resume(mut self, snapshot: SessionSnapshot) -> Self {
        self.resume = Some(snapshot);
        self
    }
}

#[cfg(not(target_family = "wasm"))]
impl<F> NanocodexBuilder<F>
where
    F: MakeResponsesService + Send + Sync + 'static,
    F::Service: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + Send + 'static,
    <F::Service as Service<ResponsesAttempt>>::Error: Into<NanocodexError> + Send + 'static,
    <F::Service as Service<ResponsesAttempt>>::Future: Send,
{
    /// Builds an agent from the configured [`OpenAi`] client recipe.
    ///
    /// Each root, spawned sibling, and fork receives a fresh concrete Tower
    /// service, tool runtime, event stream, and mutable conversation state.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid agent policy or, on native targets, when
    /// no Tokio runtime is active.
    pub fn build(self) -> Result<(Nanocodex, AgentEvents)> {
        build(self)
    }
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl<F> NanocodexBuilder<F>
where
    F: MakeResponsesService + 'static,
    F::Service: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + 'static,
    <F::Service as Service<ResponsesAttempt>>::Error: Into<NanocodexError> + 'static,
{
    /// Builds an agent from the configured [`OpenAi`] client recipe.
    ///
    /// Each root, spawned sibling, and fork receives a fresh concrete Tower
    /// service, tool runtime, event stream, and mutable conversation state.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid agent policy.
    pub fn build(self) -> Result<(Nanocodex, AgentEvents)> {
        build(self)
    }
}

fn build<F>(builder: NanocodexBuilder<F>) -> Result<(Nanocodex, AgentEvents)>
where
    F: MakeResponsesService + AgentFactory + 'static,
    F::Service:
        Service<ResponsesAttempt, Response = ResponsesServiceResponse> + AgentSend + 'static,
    <F::Service as Service<ResponsesAttempt>>::Error: Into<NanocodexError> + AgentSend + 'static,
    <F::Service as Service<ResponsesAttempt>>::Future: AgentSend,
{
    validate(&builder.config, builder.prompt_cache.key.as_deref())?;
    let config = Arc::new(builder.config);
    let factory = builder.factory;
    let service_factory: ServiceFactory<F::Service> = Arc::new({
        let service_config = Arc::clone(&config);
        move || factory.make(Arc::clone(&service_config))
    });
    build_agent(
        config,
        builder.tools,
        builder.workspace,
        builder.session_id,
        builder.prompt_cache,
        builder.codex,
        builder.resume,
        service_factory,
    )
}
