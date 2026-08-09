use super::*;
use crate::error::PersistRolloutFailure;

#[derive(Clone, Default)]
pub(in crate::agent) struct DriverFailure {
    state: Arc<std::sync::Mutex<Option<PersistRolloutFailure>>>,
}

impl DriverFailure {
    pub(in crate::agent) fn check(&self) -> Result<()> {
        self.error().map_or(Ok(()), Err)
    }

    pub(in crate::agent) fn record(&self, failure: PersistRolloutFailure) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.get_or_insert(failure);
    }

    pub(in crate::agent) fn error(&self) -> Option<NanocodexError> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.clone().map(PersistRolloutFailure::into_error)
    }
}

type SharedShutdownResult = std::result::Result<(), Arc<NanocodexError>>;

#[derive(Default)]
enum ShutdownPhase {
    #[default]
    Running,
    Requested,
    Complete(SharedShutdownResult),
}

#[derive(Default)]
struct ShutdownState {
    phase: ShutdownPhase,
    waiters: Vec<oneshot::Sender<SharedShutdownResult>>,
}

#[derive(Clone, Default)]
pub(in crate::agent) struct DriverShutdown {
    state: Arc<std::sync::Mutex<ShutdownState>>,
}

impl DriverShutdown {
    pub(in crate::agent) fn request(&self) -> (bool, oneshot::Receiver<SharedShutdownResult>) {
        let (result, receiver) = oneshot::channel();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        match &state.phase {
            ShutdownPhase::Running => {
                state.phase = ShutdownPhase::Requested;
                state.waiters.push(result);
                (true, receiver)
            }
            ShutdownPhase::Requested => {
                state.waiters.push(result);
                (false, receiver)
            }
            ShutdownPhase::Complete(outcome) => {
                drop(result.send(outcome.clone()));
                (false, receiver)
            }
        }
    }

    pub(in crate::agent) fn requested(&self) -> bool {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        !matches!(state.phase, ShutdownPhase::Running)
    }

    pub(in crate::agent) fn complete(&self, outcome: Result<()>) {
        let outcome = outcome.map_err(Arc::new);
        let waiters = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.phase = ShutdownPhase::Complete(outcome.clone());
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            drop(waiter.send(outcome.clone()));
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

pub(super) async fn begin_fail_stop(
    commands: &mut mpsc::Receiver<Command>,
    queued_turns: &mut VecDeque<QueuedTurn>,
    failure: &DriverFailure,
) {
    commands.close();
    while let Some(command) = commands.recv().await {
        fail_command(command, failure);
    }
    for queued in std::mem::take(queued_turns) {
        match queued {
            QueuedTurn::Pending { result, .. } | QueuedTurn::Cancelled { result, .. } => {
                drop(result.send(Err(failure_error(failure))));
            }
        }
    }
}

fn fail_command(command: Command, failure: &DriverFailure) {
    match command {
        Command::Prompt { result, .. } => {
            drop(result.send(Err(failure_error(failure))));
        }
        Command::RoutePrompt {
            route_result,
            turn_result,
            ..
        } => {
            drop(route_result.send(Err(failure_error(failure))));
            drop(turn_result.send(Err(failure_error(failure))));
        }
        Command::Fork { result, .. } | Command::Spawn { result } => {
            drop(result.send(Err(failure_error(failure))));
        }
        Command::Steer { result, .. }
        | Command::Cancel { result, .. }
        | Command::SetThinking { result, .. }
        | Command::SetFastMode { result, .. }
        | Command::Compact { result, .. } => {
            drop(result.send(Err(failure_error(failure))));
        }
        Command::AppendDeveloperMessage { result, .. } => {
            drop(result.send(Err(failure_error(failure))));
        }
        Command::Shutdown => {}
    }
}

fn failure_error(failure: &DriverFailure) -> NanocodexError {
    failure
        .error()
        .expect("a failed agent driver always records its persistence error")
}

pub(super) async fn begin_shutdown(
    commands: &mut mpsc::Receiver<Command>,
    queued_turns: &mut VecDeque<QueuedTurn>,
    default_thinking: Thinking,
    default_fast_mode: bool,
) {
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
            Command::RoutePrompt {
                route_result,
                turn_result,
                ..
            } => {
                drop(route_result.send(Err(NanocodexError::AgentStopped)));
                drop(turn_result);
            }
            Command::Fork { result, .. } | Command::Spawn { result } => {
                drop(result.send(Err(NanocodexError::AgentStopped)));
            }
            Command::AppendDeveloperMessage { result, .. } => {
                drop(result.send(Err(NanocodexError::AgentStopped)));
            }
            Command::Steer { result, .. }
            | Command::Cancel { result, .. }
            | Command::SetThinking { result, .. }
            | Command::SetFastMode { result, .. }
            | Command::Compact { result, .. } => {
                drop(result.send(Err(NanocodexError::AgentStopped)));
            }
            Command::Shutdown => {}
        }
    }
    mark_all_queued_turns_cancelled(queued_turns);
}

#[derive(Clone, Copy)]
pub(super) struct TurnDefaults {
    pub(super) model: Model,
    pub(super) thinking: Thinking,
    pub(super) fast_mode: bool,
}

pub(super) async fn handle_idle_command<S>(
    command: Command,
    latest: Option<&Arc<CommittedSession>>,
    spawner: &BranchSpawner<S>,
    defaults: TurnDefaults,
    session_id: &str,
    workspace: Option<Arc<str>>,
) where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + AgentSend + 'static,
    S::Error: Into<ResponseError> + AgentSend + 'static,
    S::Future: AgentSend,
{
    match command {
        Command::Fork { checkpoint, result } => {
            let checkpoint = checkpoint.or_else(|| latest.cloned());
            let outcome = match checkpoint {
                Some(checkpoint) => {
                    spawner
                        .spawn_fork(
                            checkpoint,
                            session_id,
                            defaults.model,
                            defaults.thinking,
                            defaults.fast_mode,
                        )
                        .await
                }
                None => Err(NanocodexError::ForkBeforeCompletedTurn),
            };
            drop(result.send(outcome));
        }
        Command::Spawn { result } => {
            drop(result.send(spawner.spawn_clean(
                workspace,
                session_id,
                defaults.model,
                defaults.thinking,
                defaults.fast_mode,
            )));
        }
        Command::Steer { result, .. } => {
            drop(result.send(Err(NanocodexError::TurnNotSteerable)));
        }
        Command::RoutePrompt {
            route_result,
            turn_result,
            ..
        } => {
            drop(route_result.send(Err(NanocodexError::AgentStopped)));
            drop(turn_result);
        }
        Command::Cancel { result, .. } => {
            drop(result.send(Err(NanocodexError::TurnNotCancellable)));
        }
        Command::SetThinking { result, .. } | Command::SetFastMode { result, .. } => {
            drop(result.send(Ok(())));
        }
        Command::Shutdown => {}
        Command::Compact { result, .. } => {
            drop(result.send(Err(NanocodexError::AgentStopped)));
        }
        Command::AppendDeveloperMessage { result, .. } => {
            drop(result.send(Err(NanocodexError::AgentStopped)));
        }
        Command::Prompt { .. } => {}
    }
}
