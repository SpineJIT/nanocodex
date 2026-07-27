use super::*;

/// Sole owner of mutable run state and the Responses service stack.
pub(super) struct AgentDriver<S> {
    pub(super) commands: mpsc::Receiver<Command>,
    pub(super) events: EventSink,
    pub(super) client: ResponsesClient<S>,
    pub(super) transport_stats: Arc<TransportStats>,
    pub(super) tools: Tools,
    pub(super) workspace: Option<Arc<str>>,
    pub(super) spawner: BranchSpawner<S>,
    pub(super) initial_model: Option<PreparedCheckpoint>,
    pub(super) origin: AgentOrigin,
    pub(super) durability: Durability,
}

pub(super) struct BranchSpawner<S> {
    pub(super) config: Arc<ModelConfig>,
    pub(super) tools: ToolsConfiguration,
    pub(super) lineage_id: Arc<str>,
    pub(super) prompt_cache_key: Option<Arc<str>>,
    pub(super) shared_prompt_cache: Option<SharedPromptCache>,
    pub(super) context_config: ContextSourceConfig,
    pub(super) context_source: ContextSource,
    pub(super) depth: u32,
    pub(super) durability: DurabilityConfig,
    pub(super) service_factory: ServiceFactory<S>,
}

#[derive(Clone)]
pub(super) struct AgentOrigin {
    pub(super) kind: &'static str,
    pub(super) depth: u32,
    pub(super) parent_session_id: Option<Arc<str>>,
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
            depth: self.depth,
            durability: self.durability.for_new_thread(),
            service_factory: Arc::clone(&self.service_factory),
        }
    }
}

impl<S> AgentDriver<S>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + AgentSend + 'static,
    S::Error: Into<NanocodexError> + AgentSend + 'static,
    S::Future: AgentSend,
{
    /// Drives queued turns until every command handle is dropped.
    ///
    /// # Errors
    ///
    /// Returns an infrastructure error while receiving or starting a command.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn run(mut self) -> Result<()> {
        let session_id = self.events.request_id().to_owned();
        let mut default_thinking = self.spawner.config.thinking;
        let mut default_fast_mode = self.spawner.config.fast_mode;
        let inherited_checkpoint = self.initial_model.as_ref().map(|initial| {
            Arc::new(CommittedSession::new(
                Arc::clone(&self.spawner.lineage_id),
                initial.checkpoint.clone(),
            ))
        });
        let prompt_cache_key = self
            .spawner
            .prompt_cache_key
            .as_ref()
            .map_or_else(|| Arc::clone(&self.spawner.lineage_id), Arc::clone);
        let prompt_cache =
            ModelPromptCache::new(prompt_cache_key, self.spawner.shared_prompt_cache.clone());
        let mut model = if let Some(initial) = self.initial_model.take() {
            ModelRun::from_checkpoint(
                self.events.clone(),
                Arc::clone(&self.spawner.config),
                self.client,
                Arc::clone(&self.transport_stats),
                self.tools.clone(),
                prompt_cache.clone(),
                initial,
            )
        } else {
            ModelRun::new(
                self.events.clone(),
                Arc::clone(&self.spawner.config),
                self.client,
                Arc::clone(&self.transport_stats),
                self.tools.clone(),
                prompt_cache.clone(),
                self.spawner.context_source.clone(),
            )
        };
        let mut turn_index = 0_u64;
        let mut latest_fork_checkpoint = inherited_checkpoint;
        let mut queued_turns = VecDeque::new();
        let mut commands_open = true;
        loop {
            let command = loop {
                if let Some(queued) = queued_turns.pop_front() {
                    match queued {
                        QueuedTurn::Pending {
                            key,
                            prompt,
                            thinking,
                            fast_mode,
                            parent,
                            events,
                            result,
                        } => {
                            break Command::Prompt {
                                key,
                                prompt,
                                thinking: Some(thinking),
                                fast_mode: Some(fast_mode),
                                parent,
                                events,
                                result,
                            };
                        }
                        QueuedTurn::Cancelled {
                            prompt,
                            thinking,
                            fast_mode,
                            parent,
                            events,
                            result,
                        } => {
                            turn_index += 1;
                            let prompt_content = tracing::enabled!(
                                target: "nanocodex",
                                tracing::Level::INFO
                            )
                            .then(|| serde_json::to_string(&prompt).ok())
                            .flatten();
                            let turn_span = agent_turn_span(
                                parent.as_ref(),
                                session_id.as_str(),
                                self.spawner.lineage_id.as_ref(),
                                &self.origin,
                                ReasoningSettings {
                                    mode: self.spawner.config.reasoning_mode,
                                    effort: thinking,
                                },
                                turn_index,
                                prompt.instruction.text_bytes(),
                            );
                            drop(parent);
                            turn_span.record("status", "cancelled");
                            turn_span.record("otel.status_code", "ERROR");
                            if let Some(prompt_content) = &prompt_content {
                                turn_span.in_scope(|| {
                                    info!(
                                        target: "nanocodex",
                                        content_kind = "prompt",
                                        content = prompt_content.as_str(),
                                        "turn content"
                                    );
                                });
                            }
                            let _guard = turn_span.enter();
                            model.set_events(events);
                            model.emit_cancelled_before_start(
                                &prompt,
                                self.workspace.as_deref(),
                                thinking,
                                fast_mode,
                            )?;
                            model.set_events(self.events.clone());
                            drop(result.send(Err(NanocodexError::TurnCancelled)));
                            continue;
                        }
                    }
                }
                if commands_open {
                    let Some(command) = self.commands.recv().await else {
                        commands_open = false;
                        continue;
                    };
                    break command;
                }
                model.shutdown().await;
                return Ok(());
            };
            let Command::Prompt {
                key,
                prompt,
                thinking,
                fast_mode,
                parent,
                events,
                result,
            } = command
            else {
                if let Command::SetThinking { thinking, result } = command {
                    default_thinking = thinking;
                    drop(result.send(Ok(())));
                    continue;
                }
                if let Command::SetFastMode { enabled, result } = command {
                    default_fast_mode = enabled;
                    drop(result.send(Ok(())));
                    continue;
                }
                handle_idle_command(
                    command,
                    latest_fork_checkpoint.as_ref(),
                    &self.spawner,
                    default_thinking,
                    default_fast_mode,
                    session_id.as_str(),
                    self.workspace.clone(),
                );
                continue;
            };
            let thinking = thinking.unwrap_or(default_thinking);
            let fast_mode = fast_mode.unwrap_or(default_fast_mode);
            turn_index += 1;
            let prompt_content = tracing::enabled!(
                target: "nanocodex",
                tracing::Level::INFO
            )
            .then(|| serde_json::to_string(&prompt).ok())
            .flatten();
            let turn_span = agent_turn_span(
                parent.as_ref(),
                session_id.as_str(),
                self.spawner.lineage_id.as_ref(),
                &self.origin,
                ReasoningSettings {
                    mode: self.spawner.config.reasoning_mode,
                    effort: thinking,
                },
                turn_index,
                prompt.instruction.text_bytes(),
            );
            drop(parent);
            if let Some(prompt_content) = &prompt_content {
                turn_span.in_scope(|| {
                    info!(
                        target: "nanocodex",
                        content_kind = "prompt",
                        content = prompt_content.as_str(),
                        "turn content"
                    );
                });
            }
            let durability_turn = self.durability.start_turn(&prompt);
            let (steers, steer_rx) = mpsc::channel(STEER_CAPACITY);
            let (cancel, cancel_rx) = oneshot::channel();
            let (fork_snapshots, mut fork_snapshot_rx) = watch::channel(None);
            let mut fork_snapshots_open = true;
            let mut cancel = Some(cancel);
            let mut cancel_result = None;
            model.set_events(events);
            let mut execution = Box::pin(
                model
                    .execute(
                        prompt,
                        self.workspace.clone(),
                        thinking,
                        fast_mode,
                        steer_rx,
                        cancel_rx,
                        fork_snapshots,
                    )
                    .instrument(turn_span.clone()),
            );
            let completed = loop {
                if !commands_open {
                    break execution.as_mut().await;
                }
                tokio::select! {
                    biased;
                    changed = fork_snapshot_rx.changed(), if fork_snapshots_open => {
                        if changed.is_err() {
                            fork_snapshots_open = false;
                            continue;
                        }
                        let snapshot = fork_snapshot_rx.borrow_and_update().clone();
                        if let Some(snapshot) = snapshot {
                            latest_fork_checkpoint = Some(Arc::new(CommittedSession::new(
                                Arc::clone(&self.spawner.lineage_id),
                                snapshot,
                            )));
                        }
                    }
                    outcome = &mut execution => break outcome,
                    command = self.commands.recv() => {
                        match command {
                            Some(Command::Prompt {
                                key,
                                prompt,
                                thinking: _,
                                fast_mode: _,
                                parent,
                                events,
                                result,
                            }) => {
                                queued_turns.push_back(QueuedTurn::Pending {
                                    key,
                                    prompt,
                                    thinking: default_thinking,
                                    fast_mode: default_fast_mode,
                                    parent,
                                    events,
                                    result,
                                });
                            }
                            Some(Command::Steer { key: target, prompt, result }) => {
                                if target != key {
                                    drop(result.send(Err(NanocodexError::TurnNotSteerable)));
                                    continue;
                                }
                                let outcome = steers.try_send(prompt).map_err(|error| match error {
                                    mpsc::error::TrySendError::Full(_) => {
                                        NanocodexError::SteerQueueFull
                                    }
                                    mpsc::error::TrySendError::Closed(_) => {
                                        NanocodexError::TurnNotSteerable
                                    }
                                });
                                drop(result.send(outcome));
                            }
                            Some(Command::Cancel { key: target, result: cancellation }) => {
                                if target != key {
                                    if cancel_queued_turn(&mut queued_turns, target) {
                                        drop(cancellation.send(Ok(())));
                                    } else {
                                        drop(cancellation.send(Err(
                                            NanocodexError::TurnNotCancellable,
                                        )));
                                    }
                                    continue;
                                }
                                let Some(cancel) = cancel.take() else {
                                    drop(cancellation.send(Err(
                                        NanocodexError::TurnNotCancellable,
                                    )));
                                    continue;
                                };
                                let _ = cancel.send(());
                                cancel_result = Some(cancellation);
                                break execution.as_mut().await;
                            }
                            Some(command @ (Command::Fork { .. } | Command::Spawn { .. })) => {
                                if let Some(snapshot) =
                                    fork_snapshot_rx.borrow_and_update().clone()
                                {
                                    latest_fork_checkpoint =
                                        Some(Arc::new(CommittedSession::new(
                                            Arc::clone(&self.spawner.lineage_id),
                                            snapshot,
                                        )));
                                }
                                handle_idle_command(
                                    command,
                                    latest_fork_checkpoint.as_ref(),
                                    &self.spawner,
                                    default_thinking,
                                    default_fast_mode,
                                    session_id.as_str(),
                                    self.workspace.clone(),
                                );
                            }
                            Some(Command::SetThinking { thinking, result }) => {
                                default_thinking = thinking;
                                drop(result.send(Ok(())));
                            }
                            Some(Command::SetFastMode { enabled, result }) => {
                                default_fast_mode = enabled;
                                drop(result.send(Ok(())));
                            }
                            None => {
                                commands_open = false;
                                queued_turns.clear();
                                if let Some(cancel) = cancel.take() {
                                    let _ = cancel.send(());
                                }
                            }
                        }
                    }
                }
            };
            drop(execution);
            model.set_events(self.events.clone());
            let (outcome, was_cancelled): (Result<TurnResult>, bool) = match completed {
                Ok(ModelTurnOutcome::Completed(completed)) => {
                    let CompletedModelTurn {
                        final_message,
                        usage,
                        checkpoint,
                    } = completed;
                    let checkpoint = Arc::new(CommittedSession::new(
                        Arc::clone(&self.spawner.lineage_id),
                        checkpoint,
                    ));
                    let durability_turn = durability_turn.completed(final_message.clone());
                    self.durability
                        .persist(&checkpoint, durability_turn)
                        .instrument(turn_span.clone())
                        .await;
                    latest_fork_checkpoint = Some(Arc::clone(&checkpoint));
                    (
                        Ok(TurnResult {
                            final_message,
                            usage,
                            checkpoint,
                        }),
                        false,
                    )
                }
                Ok(ModelTurnOutcome::Cancelled(checkpoint)) => {
                    let checkpoint = Arc::new(CommittedSession::new(
                        Arc::clone(&self.spawner.lineage_id),
                        checkpoint,
                    ));
                    let durability_turn = durability_turn.interrupted();
                    self.durability
                        .persist(&checkpoint, durability_turn)
                        .instrument(turn_span.clone())
                        .await;
                    latest_fork_checkpoint = Some(Arc::clone(&checkpoint));
                    let prepared = prepare_checkpoint(
                        checkpoint.model().clone(),
                        &self.spawner.config,
                        &self.tools,
                        self.spawner.context_source.clone(),
                    );
                    model = ModelRun::from_checkpoint(
                        self.events.clone(),
                        Arc::clone(&self.spawner.config),
                        ResponsesClient::new((self.spawner.service_factory)()),
                        Arc::clone(&self.transport_stats),
                        self.tools.clone(),
                        prompt_cache.clone(),
                        prepared,
                    );
                    (Err(NanocodexError::TurnCancelled), true)
                }
                Err(error) => (Err(error), false),
            };
            turn_span.record(
                "status",
                if was_cancelled {
                    "cancelled"
                } else if outcome.is_ok() {
                    "completed"
                } else {
                    "failed"
                },
            );
            turn_span.record(
                "otel.status_code",
                if outcome.is_ok() { "OK" } else { "ERROR" },
            );
            drop(result.send(outcome));
            if let Some(cancel_result) = cancel_result {
                let outcome = if was_cancelled {
                    Ok(())
                } else {
                    Err(NanocodexError::TurnNotCancellable)
                };
                drop(cancel_result.send(outcome));
            }
        }
    }
}

fn agent_turn_span(
    parent: Option<&tracing::Span>,
    session_id: &str,
    lineage_id: &str,
    origin: &AgentOrigin,
    reasoning: ReasoningSettings,
    turn_index: u64,
    prompt_bytes: usize,
) -> tracing::Span {
    let parent_id = parent.and_then(tracing::Span::id);
    let parented = parent_id.is_some();
    let span = info_span!(
        target: "nanocodex",
        parent: parent_id,
        "agent.turn",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        session.id = session_id,
        session.lineage_id = lineage_id,
        parent.session.id = tracing::field::Empty,
        agent.origin = origin.kind,
        agent.depth = origin.depth,
        trace.parented = parented,
        model = nanocodex_oai_api::MODEL,
        reasoning.mode = reasoning.mode.as_str(),
        reasoning.effort = reasoning.effort.as_str(),
        thinking = reasoning.effort.as_str(),
        turn.index = turn_index,
        prompt.bytes = prompt_bytes,
        usage.input_tokens = tracing::field::Empty,
        usage.cached_input_tokens = tracing::field::Empty,
        usage.cache_write_input_tokens = tracing::field::Empty,
        usage.output_tokens = tracing::field::Empty,
        usage.reasoning_output_tokens = tracing::field::Empty,
        usage.total_tokens = tracing::field::Empty,
        cost.usd = tracing::field::Empty,
        cost.status = tracing::field::Empty,
        cost.service_tier = tracing::field::Empty,
        status = tracing::field::Empty,
    );
    if let Some(parent_session_id) = &origin.parent_session_id {
        span.record("parent.session.id", parent_session_id.as_ref());
    }
    span
}

#[derive(Clone, Copy)]
struct ReasoningSettings {
    mode: ReasoningMode,
    effort: Thinking,
}

fn cancel_queued_turn(queued_turns: &mut VecDeque<QueuedTurn>, target: TurnKey) -> bool {
    let Some(position) = queued_turns
        .iter()
        .position(|queued| matches!(queued, QueuedTurn::Pending { key, .. } if *key == target))
    else {
        return false;
    };
    let Some(queued) = queued_turns.remove(position) else {
        return false;
    };
    let QueuedTurn::Pending {
        prompt,
        thinking,
        fast_mode,
        parent,
        events,
        result,
        ..
    } = queued
    else {
        return false;
    };
    queued_turns.insert(
        position,
        QueuedTurn::Cancelled {
            prompt,
            thinking,
            fast_mode,
            parent,
            events,
            result,
        },
    );
    true
}

fn handle_idle_command<S>(
    command: Command,
    latest: Option<&Arc<CommittedSession>>,
    spawner: &BranchSpawner<S>,
    thinking: Thinking,
    fast_mode: bool,
    session_id: &str,
    workspace: Option<Arc<str>>,
) where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + AgentSend + 'static,
    S::Error: Into<NanocodexError> + AgentSend + 'static,
    S::Future: AgentSend,
{
    match command {
        Command::Fork { checkpoint, result } => {
            let checkpoint = checkpoint.or_else(|| latest.cloned());
            let outcome = checkpoint
                .ok_or(NanocodexError::ForkBeforeCompletedTurn)
                .and_then(|checkpoint| {
                    spawner.spawn_fork(&checkpoint, session_id, thinking, fast_mode)
                });
            drop(result.send(outcome));
        }
        Command::Spawn { result } => {
            drop(result.send(spawner.spawn_clean(workspace, session_id, thinking, fast_mode)));
        }
        Command::Steer { result, .. } => {
            drop(result.send(Err(NanocodexError::TurnNotSteerable)));
        }
        Command::Cancel { result, .. } => {
            drop(result.send(Err(NanocodexError::TurnNotCancellable)));
        }
        Command::SetThinking { result, .. } | Command::SetFastMode { result, .. } => {
            drop(result.send(Ok(())));
        }
        Command::Prompt { .. } => {}
    }
}

impl<S> BranchSpawner<S>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + AgentSend + 'static,
    S::Error: Into<NanocodexError> + AgentSend + 'static,
    S::Future: AgentSend,
{
    fn spawn_fork(
        &self,
        checkpoint: &CommittedSession,
        parent_session_id: &str,
        thinking: Thinking,
        fast_mode: bool,
    ) -> Result<(Nanocodex, AgentEvents)> {
        let session_id = SessionId::new();
        let workspace = Some(Arc::<str>::from(checkpoint.model().workspace()));
        let mut spawner = self.clone();
        let mut config = (*spawner.config).clone();
        config.thinking = thinking;
        config.fast_mode = fast_mode;
        spawner.config = Arc::new(config);
        spawner.depth = self.depth.saturating_add(1);
        spawn_agent_driver(
            spawner,
            session_id,
            workspace,
            (self.service_factory)(),
            Some(InitialResume::Exact(Box::new(checkpoint.model().clone()))),
            AgentOrigin {
                kind: "fork",
                depth: self.depth.saturating_add(1),
                parent_session_id: Some(Arc::from(parent_session_id)),
            },
        )
    }

    fn spawn_clean(
        &self,
        workspace: Option<Arc<str>>,
        parent_session_id: &str,
        thinking: Thinking,
        fast_mode: bool,
    ) -> Result<(Nanocodex, AgentEvents)> {
        let session_id = SessionId::new();
        let session_id_text = session_id.to_string();
        let depth = self.depth.saturating_add(1);
        let mut config = (*self.config).clone();
        config.thinking = thinking;
        config.fast_mode = fast_mode;
        let spawner = Self {
            config: Arc::new(config),
            tools: self.tools.clone(),
            lineage_id: Arc::from(session_id_text.as_str()),
            prompt_cache_key: self.prompt_cache_key.as_ref().map(Arc::clone),
            shared_prompt_cache: self.shared_prompt_cache.clone(),
            context_config: self.context_config.clone(),
            context_source: self.context_config.build(),
            depth,
            durability: self.durability.for_new_thread(),
            service_factory: Arc::clone(&self.service_factory),
        };
        let service = (self.service_factory)();
        spawn_agent_driver(
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
        )
    }
}
