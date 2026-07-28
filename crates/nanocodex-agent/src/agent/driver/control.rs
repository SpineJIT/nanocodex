use super::*;

#[derive(Clone, Default)]
pub(in crate::agent) struct DriverShutdown {
    result: Arc<std::sync::Mutex<Option<oneshot::Sender<Result<()>>>>>,
}

impl DriverShutdown {
    fn request(&self, result: oneshot::Sender<Result<()>>) -> bool {
        let mut waiting = match self.result.lock() {
            Ok(waiting) => waiting,
            Err(poisoned) => poisoned.into_inner(),
        };
        if waiting.is_some() {
            return false;
        }
        *waiting = Some(result);
        true
    }

    pub(in crate::agent) fn requested(&self) -> bool {
        let waiting = match self.result.lock() {
            Ok(waiting) => waiting,
            Err(poisoned) => poisoned.into_inner(),
        };
        waiting.is_some()
    }

    pub(in crate::agent) fn complete(&self, outcome: Result<()>) {
        let result = {
            let mut waiting = match self.result.lock() {
                Ok(waiting) => waiting,
                Err(poisoned) => poisoned.into_inner(),
            };
            waiting.take()
        };
        if let Some(result) = result {
            drop(result.send(outcome));
        }
    }
}

pub(super) fn cancel_queued_turn(queued_turns: &mut VecDeque<QueuedTurn>, target: TurnKey) -> bool {
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

pub(super) fn mark_all_queued_turns_cancelled(queued_turns: &mut VecDeque<QueuedTurn>) {
    let accepted = std::mem::take(queued_turns);
    queued_turns.extend(accepted.into_iter().map(|queued| match queued {
        QueuedTurn::Pending {
            prompt,
            thinking,
            fast_mode,
            parent,
            events,
            result,
            ..
        } => QueuedTurn::Cancelled {
            prompt,
            thinking,
            fast_mode,
            parent,
            events,
            result,
        },
        queued @ QueuedTurn::Cancelled { .. } => queued,
    }));
}

pub(super) async fn begin_shutdown(
    commands: &mut mpsc::Receiver<Command>,
    queued_turns: &mut VecDeque<QueuedTurn>,
    default_thinking: Thinking,
    default_fast_mode: bool,
    shutdown: &DriverShutdown,
    result: oneshot::Sender<Result<()>>,
) {
    if !shutdown.request(result) {
        return;
    }
    commands.close();
    while let Some(command) = commands.recv().await {
        match command {
            Command::Prompt {
                key,
                prompt,
                thinking,
                fast_mode,
                parent,
                events,
                result,
            } => {
                queued_turns.push_back(QueuedTurn::Pending {
                    key,
                    prompt,
                    thinking: thinking.unwrap_or(default_thinking),
                    fast_mode: fast_mode.unwrap_or(default_fast_mode),
                    parent,
                    events,
                    result,
                });
            }
            Command::Fork { result, .. } | Command::Spawn { result } => {
                drop(result.send(Err(NanocodexError::AgentStopped)));
            }
            Command::Steer { result, .. }
            | Command::Cancel { result, .. }
            | Command::SetThinking { result, .. }
            | Command::SetFastMode { result, .. }
            | Command::Compact { result, .. }
            | Command::Shutdown { result } => {
                drop(result.send(Err(NanocodexError::AgentStopped)));
            }
        }
    }
    mark_all_queued_turns_cancelled(queued_turns);
}

pub(super) fn handle_idle_command<S>(
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
        Command::Shutdown { result } => {
            drop(result.send(Err(NanocodexError::AgentStopped)));
        }
        Command::Compact { result, .. } => {
            drop(result.send(Err(NanocodexError::AgentStopped)));
        }
        Command::Prompt { .. } => {}
    }
}
