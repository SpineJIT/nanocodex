use super::*;

pub(super) struct SpineFamily {
    resources: SpineResources,
    agents: BTreeMap<String, Nanocodex>,
    event_tasks: Vec<JoinHandle<()>>,
    updates: mpsc::UnboundedSender<WorkerEvent>,
}

impl SpineFamily {
    pub(super) const fn empty(updates: mpsc::UnboundedSender<WorkerEvent>) -> Self {
        Self {
            resources: SpineResources {
                _realtime: None,
                child_agents: None,
                mpp_adapter: None,
                _mcp: None,
                browser: None,
                vm: None,
            },
            agents: BTreeMap::new(),
            event_tasks: Vec::new(),
            updates,
        }
    }

    pub(super) fn new(
        configured: ConfiguredAgent,
        updates: mpsc::UnboundedSender<WorkerEvent>,
    ) -> Self {
        let ConfiguredAgent {
            handle,
            events,
            realtime,
            child_agents,
            mpp_adapter,
            mcp,
            browser,
            vm,
        } = configured;
        let root_session_id = handle.session_id().to_string();
        let mut family = Self {
            agents: BTreeMap::from([(root_session_id, handle)]),
            resources: SpineResources {
                _realtime: realtime,
                child_agents,
                mpp_adapter,
                _mcp: mcp,
                browser,
                vm,
            },
            event_tasks: Vec::new(),
            updates,
        };
        family.forward_child_events(events);
        family
    }

    pub(super) fn agent(&self, session_id: &str) -> Result<Nanocodex> {
        self.agents
            .get(session_id)
            .cloned()
            .ok_or_else(|| eyre!("Spine session {session_id} is not available"))
    }

    pub(super) fn contains(&self, session_id: &str) -> bool {
        self.agents.contains_key(session_id)
    }

    pub(super) async fn fork(&mut self, parent_session_id: &str) -> Result<String> {
        let parent = self.agent(parent_session_id)?;
        let (agent, events) = parent.fork().await?;
        let session_id = agent.session_id().to_string();
        self.agents.insert(session_id.clone(), agent);
        self.forward_child_events(events);
        Ok(session_id)
    }

    pub(super) async fn shutdown_closed(&mut self, session_id: &str) {
        if let Some(agent) = self.agents.remove(session_id)
            && let Err(error) = agent.shutdown().await
        {
            let _ = self.updates.send(WorkerEvent::SpineTreeFailed {
                error: format!("closed Spine session cleanup failed: {error}"),
            });
        }
    }

    pub(super) async fn set_fast_mode(&self, enabled: bool) -> Result<()> {
        for agent in self.agents.values() {
            agent.set_fast_mode(enabled).await?;
        }
        Ok(())
    }

    pub(super) async fn set_thinking(&self, thinking: Thinking) -> Result<()> {
        for agent in self.agents.values() {
            agent.set_thinking(thinking).await?;
        }
        Ok(())
    }

    pub(super) async fn shutdown(mut self) -> Result<()> {
        let mut first_error: Option<eyre::Report> = None;
        let agents = std::mem::take(&mut self.agents);
        for agent in agents.values() {
            if let Err(error) = agent.shutdown().await
                && first_error.is_none()
            {
                first_error = Some(error.into());
            }
        }
        drop(agents);
        for task in self.event_tasks.drain(..) {
            let _ = task.await;
        }
        if let Some(child_agents) = self.resources.child_agents {
            child_agents.shutdown().await;
        }
        if let Some(browser) = self.resources.browser
            && let Err(error) = browser.shutdown().await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Some(vm) = self.resources.vm
            && let Err(error) = vm.shutdown().await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Some(adapter) = self.resources.mpp_adapter
            && let Err(error) = adapter.shutdown().await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn forward_child_events(&mut self, mut events: AgentEvents) {
        let updates = self.updates.clone();
        self.event_tasks.push(tokio::spawn(async move {
            while let Some(event) = events.recv_timed().await {
                if updates.send(WorkerEvent::RootAgentEvent { event }).is_err() {
                    return;
                }
            }
        }));
    }
}

struct SpineResources {
    _realtime: Option<nanocodex::OpenAi>,
    child_agents: Option<Arc<ChildAgents>>,
    mpp_adapter: Option<MppAdapter>,
    _mcp: Option<McpHandle>,
    browser: Option<ConfiguredBrowser>,
    vm: Option<ConfiguredVm>,
}
