use super::*;

pub(in crate::agent) struct BranchSpawner<S> {
    pub(in crate::agent) config: Arc<ModelConfig>,
    pub(in crate::agent) tools: ToolsConfiguration,
    pub(in crate::agent) lineage_id: Arc<str>,
    pub(in crate::agent) prompt_cache_key: Option<Arc<str>>,
    pub(in crate::agent) shared_prompt_cache: Option<SharedPromptCache>,
    pub(in crate::agent) context_config: ContextSourceConfig,
    pub(in crate::agent) context_source: ContextSource,
    pub(in crate::agent) tool_profile: ToolProfile,
    pub(in crate::agent) depth: u32,
    pub(in crate::agent) durability: DurabilityConfig,
    pub(in crate::agent) service_factory: ServiceFactory<S>,
}

#[derive(Clone)]
pub(in crate::agent) struct AgentOrigin {
    pub(in crate::agent) kind: &'static str,
    pub(in crate::agent) depth: u32,
    pub(in crate::agent) parent_session_id: Option<Arc<str>>,
}

impl<S> Clone for BranchSpawner<S> {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            tools: self.tools.clone(),
            lineage_id: Arc::clone(&self.lineage_id),
            prompt_cache_key: self.prompt_cache_key.as_ref().map(Arc::clone),
            shared_prompt_cache: self.shared_prompt_cache.clone(),
            context_config: self.context_config.clone(),
            context_source: self.context_source.clone(),
            tool_profile: self.tool_profile,
            depth: self.depth,
            durability: self.durability.for_new_thread(),
            service_factory: Arc::clone(&self.service_factory),
        }
    }
}

impl<S> BranchSpawner<S>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + AgentSend + 'static,
    S::Error: Into<ResponseError> + AgentSend + 'static,
    S::Future: AgentSend,
{
    pub(super) async fn spawn_fork(
        &self,
        checkpoint: Arc<CommittedSession>,
        parent_session_id: &str,
        model: Model,
        thinking: Thinking,
        fast_mode: bool,
        tool_profile: ToolProfile,
    ) -> Result<(Nanocodex, AgentEvents)> {
        let session_id = SessionId::new();
        let session_id_text = session_id.to_string();
        let workspace = Some(Arc::<str>::from(checkpoint.model().workspace()));
        let mut spawner = self.clone();
        spawner.context_source = spawner.context_config.build();
        let mut config = (*spawner.config).clone();
        config.model = model;
        config.thinking = thinking;
        config.fast_mode = fast_mode;
        spawner.config = Arc::new(config);
        let profile_changes = self.tool_profile != tool_profile && self.tools.has_child_factory();
        spawner.tool_profile = tool_profile;
        if profile_changes {
            spawner.prompt_cache_key = Some(Arc::from(session_id_text.as_str()));
        }
        spawner.depth = self.depth.saturating_add(1);
        let service = (spawner.service_factory)(Arc::clone(&spawner.config));
        let initial_resume = if profile_changes {
            InitialResume::History(Box::new(HistoryCheckpoint {
                workspace: checkpoint.model().workspace().to_owned(),
                canonical_context: checkpoint.model().canonical_context().clone(),
                history: checkpoint.model().snapshot_history(),
                prompt_cache_key: spawner
                    .prompt_cache_key
                    .as_ref()
                    .map_or_else(|| Arc::from(session_id_text.as_str()), Arc::clone),
                context_baseline: None,
            }))
        } else {
            InitialResume::Exact(Box::new(checkpoint.model().clone()))
        };
        let (child, events, initial_checkpoint) = spawn_agent_driver(
            spawner,
            session_id,
            workspace,
            service,
            Some(initial_resume),
            AgentOrigin {
                kind: "fork",
                depth: self.depth.saturating_add(1),
                parent_session_id: Some(Arc::from(parent_session_id)),
            },
            tool_profile,
        )?;
        let checkpoint = if profile_changes {
            initial_checkpoint.as_ref().ok_or_else(|| {
                NanocodexError::InvalidRequest(
                    "profile-changing fork did not produce a child history checkpoint".to_owned(),
                )
            })?
        } else {
            &checkpoint
        };
        if let Err(error) = child.seed_initial_checkpoint(checkpoint, thinking).await {
            let _ = child.shutdown().await;
            child.discard_unpublished_rollout().await;
            return Err(error);
        }
        Ok((child, events))
    }

    pub(super) fn spawn_clean(
        &self,
        workspace: Option<Arc<str>>,
        parent_session_id: &str,
        model: Model,
        thinking: Thinking,
        fast_mode: bool,
        tool_profile: ToolProfile,
    ) -> Result<(Nanocodex, AgentEvents)> {
        let session_id = SessionId::new();
        let session_id_text = session_id.to_string();
        let depth = self.depth.saturating_add(1);
        let mut config = (*self.config).clone();
        config.model = model;
        config.thinking = thinking;
        config.fast_mode = fast_mode;
        let profile_changes = self.tool_profile != tool_profile && self.tools.has_child_factory();
        let prompt_cache_key = if profile_changes {
            Arc::from(session_id_text.as_str())
        } else {
            self.prompt_cache_key
                .as_ref()
                .map_or_else(|| Arc::clone(&self.lineage_id), Arc::clone)
        };
        let spawner = Self {
            config: Arc::new(config),
            tools: self.tools.clone(),
            lineage_id: Arc::from(session_id_text.as_str()),
            prompt_cache_key: Some(prompt_cache_key),
            shared_prompt_cache: self.shared_prompt_cache.clone(),
            context_config: self.context_config.clone(),
            context_source: self.context_config.build(),
            tool_profile,
            depth,
            durability: self.durability.for_new_thread(),
            service_factory: Arc::clone(&self.service_factory),
        };
        let service = (spawner.service_factory)(Arc::clone(&spawner.config));
        let (agent, events, _) = spawn_agent_driver(
            spawner,
            session_id,
            workspace,
            service,
            None,
            AgentOrigin {
                kind: "spawn",
                depth,
                parent_session_id: Some(Arc::from(parent_session_id)),
            },
            tool_profile,
        )?;
        Ok((agent, events))
    }
}
