use super::*;

#[cfg(not(target_family = "wasm"))]
use crate::rollout::RolloutInfo;

/// Cheap, cloneable command handle for an owned agent driver.
pub struct Nanocodex {
    pub(super) commands: mpsc::Sender<Command>,
    pub(super) events: EventSink,
    pub(super) next_turn: Arc<AtomicU64>,
    pub(super) lineage_id: Arc<str>,
    pub(super) session_id: SessionId,
    pub(super) durability: Durability,
}

impl Clone for Nanocodex {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            events: self.events.clone(),
            next_turn: Arc::clone(&self.next_turn),
            lineage_id: Arc::clone(&self.lineage_id),
            session_id: self.session_id,
            durability: self.durability.clone(),
        }
    }
}

/// Weak child-agent capability for the driver that owns one tool runtime.
///
/// A tools factory receives a fresh handle for every agent driver. Holding the
/// handle does not keep its agent alive.
#[derive(Clone)]
pub struct AgentHandle {
    pub(super) commands: mpsc::WeakSender<Command>,
}

impl AgentHandle {
    /// Starts a clean agent with the containing driver's private configuration,
    /// service factory, workspace policy, and per-agent tools factory.
    ///
    /// The child receives a new session, cache lineage, conversation, driver,
    /// WebSocket, and tool runtime. It does not inherit conversation history.
    ///
    /// # Errors
    ///
    /// Returns an error after the containing driver has stopped.
    pub async fn spawn(&self) -> Result<(Nanocodex, AgentEvents)> {
        let commands = self.commands()?;
        request_spawn(&commands).await
    }

    /// Forks the containing agent's latest safe model boundary.
    ///
    /// # Errors
    ///
    /// Returns an error before the first prompt reaches a safe boundary, or
    /// after the containing agent driver has stopped.
    pub async fn fork(&self) -> Result<(Nanocodex, AgentEvents)> {
        let commands = self.commands()?;
        request_fork(&commands, None).await
    }

    fn commands(&self) -> Result<mpsc::Sender<Command>> {
        self.commands.upgrade().ok_or(NanocodexError::AgentStopped)
    }
}

impl Nanocodex {
    /// Starts configuring an agent from a reusable [`OpenAi`] client recipe.
    #[must_use]
    pub fn builder<F>(openai: OpenAi<F>) -> NanocodexBuilder<F>
    where
        F: MakeResponsesService,
    {
        let (config, factory) = openai.into_parts();
        NanocodexBuilder {
            config,
            tools: ToolsConfiguration::Shared(Tools::default()),
            workspace: None,
            session_id: None,
            prompt_cache: PromptCacheConfig::default(),
            codex: CodexCompatibility::default(),
            resume: None,
            factory,
        }
    }

    /// Returns the stable identity used by events, transport metadata, and any rollout.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Returns the Codex-compatible rollout identity and path when recording is enabled.
    #[cfg(not(target_family = "wasm"))]
    #[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
    #[must_use]
    pub const fn rollout(&self) -> Option<&RolloutInfo> {
        self.durability.info()
    }

    /// Retries any pending rollout write and waits for a durable file flush.
    ///
    /// This is a no-op when rollout recording is disabled. CLI consumers call
    /// it at completed turn boundaries so persistence failures are user-visible.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured rollout cannot be written.
    #[cfg(not(target_family = "wasm"))]
    #[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
    pub async fn flush_rollout(&self) -> Result<()> {
        self.durability.flush().await
    }

    /// Accepts the agent's prompt and immediately returns its turn handle.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty prompt or if the driver stopped.
    pub async fn prompt(&self, prompt: impl Into<Prompt>) -> Result<Turn> {
        let prompt = prompt.into();
        if prompt.instruction.is_empty() {
            return Err(NanocodexError::InvalidRequest(
                "prompt instruction must not be empty".to_owned(),
            ));
        }
        let key = TurnKey(self.next_turn.fetch_add(1, Ordering::Relaxed));
        let parent = tracing::Span::current();
        let parent = (!parent.is_disabled()).then_some(parent);
        let (events, event_stream) = self.events.mirrored_channel();
        let (result, receiver) = oneshot::channel();
        if self
            .commands
            .send(Command::Prompt {
                key,
                prompt,
                thinking: None,
                fast_mode: None,
                parent,
                events,
                result,
            })
            .await
            .is_err()
        {
            return Err(NanocodexError::AgentStopped);
        }
        Ok(Turn {
            control: TurnControl {
                key,
                commands: self.commands.clone(),
            },
            events: event_stream,
            result: receiver,
        })
    }

    /// Changes the reasoning effort for subsequently accepted turns.
    ///
    /// An active turn and prompts already queued by the driver retain the
    /// effort they captured when accepted.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent driver has stopped.
    pub async fn set_thinking(&self, thinking: Thinking) -> Result<()> {
        request_command(&self.commands, |result| Command::SetThinking {
            thinking,
            result,
        })
        .await
    }

    /// Enables or disables priority processing for subsequently accepted turns.
    ///
    /// An active turn and prompts already queued by the driver retain the mode
    /// they captured when accepted.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent driver has stopped.
    pub async fn set_fast_mode(&self, enabled: bool) -> Result<()> {
        request_command(&self.commands, |result| Command::SetFastMode {
            enabled,
            result,
        })
        .await
    }

    /// Starts a clean sibling agent with the same private configuration,
    /// workspace policy, service factory, and tools factory.
    ///
    /// The sibling receives a new session, cache lineage, conversation,
    /// WebSocket, and tool runtime. It does not inherit conversation history.
    ///
    /// # Errors
    ///
    /// Returns an error after this agent's driver has stopped.
    pub async fn spawn(&self) -> Result<(Self, AgentEvents)> {
        request_spawn(&self.commands).await
    }

    /// Forks from the latest safe model boundary into an independently driven
    /// agent.
    ///
    /// The child receives a fresh WebSocket and tool runtime while sharing the
    /// immutable transcript, inherited incremental delta, and prompt-cache
    /// lineage. Partial model output and unmatched tool calls are excluded.
    ///
    /// # Errors
    ///
    /// Returns an error before the first prompt reaches a safe boundary, or
    /// when the driver has stopped.
    pub async fn fork(&self) -> Result<(Self, AgentEvents)> {
        self.request_fork(None).await
    }

    /// Forks from an exact historical completed turn while this agent may keep
    /// advancing on its current branch.
    ///
    /// # Errors
    ///
    /// Returns an error when the result belongs to another conversation or the
    /// driver stopped.
    pub async fn fork_from(&self, completed: &TurnResult) -> Result<(Self, AgentEvents)> {
        if completed.checkpoint.lineage_id() != self.lineage_id.as_ref() {
            return Err(NanocodexError::CheckpointLineageMismatch);
        }
        self.request_fork(Some(Arc::clone(&completed.checkpoint)))
            .await
    }

    async fn request_fork(
        &self,
        checkpoint: Option<Arc<CommittedSession>>,
    ) -> Result<(Self, AgentEvents)> {
        request_fork(&self.commands, checkpoint).await
    }
}

async fn request_fork(
    commands: &mpsc::Sender<Command>,
    checkpoint: Option<Arc<CommittedSession>>,
) -> Result<(Nanocodex, AgentEvents)> {
    request_command(commands, |result| Command::Fork { checkpoint, result }).await
}

async fn request_spawn(commands: &mpsc::Sender<Command>) -> Result<(Nanocodex, AgentEvents)> {
    request_command(commands, |result| Command::Spawn { result }).await
}

pub(super) async fn request_command<T>(
    commands: &mpsc::Sender<Command>,
    command: impl FnOnce(oneshot::Sender<Result<T>>) -> Command,
) -> Result<T> {
    let (result, receiver) = oneshot::channel();
    commands
        .send(command(result))
        .await
        .map_err(|_| NanocodexError::AgentStopped)?;
    receiver.await.map_err(|_| NanocodexError::AgentStopped)?
}
