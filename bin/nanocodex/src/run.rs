use clap::{Args, builder::NonEmptyStringValueParser};
use eyre::{Result, eyre};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::config::AgentArgs;
use crate::vm::VmArgs;
use crate::{
    app_core::{StandardWorkerFactory, WorkerFactory},
    tui::{WorkerCommand, WorkerEvent},
};

#[derive(Args)]
pub(crate) struct Run {
    /// Prompt submitted to the agent.
    #[arg(value_parser = NonEmptyStringValueParser::new())]
    prompt: String,

    /// Submit the same prompt as sequential follow-on turns on one owned session.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=100))]
    repeat: u16,
}

impl Run {
    pub(crate) async fn run(self, config: AgentArgs, vm: VmArgs) -> Result<()> {
        let factory = StandardWorkerFactory::build(config, vm).await?;
        let mut worker = factory.start();
        let mut stdout = tokio::io::stdout();
        let run_result: Result<()> = async {
            for prompt_id in 1..=u64::from(self.repeat) {
                let commands = worker.commands().clone();
                commands
                    .send(WorkerCommand::root_prompt(prompt_id, self.prompt.clone()))
                    .map_err(|_| eyre!("application worker stopped"))?;
                let completion = complete_turn(worker.events_mut(), &mut stdout);
                tokio::pin!(completion);
                tokio::select! {
                    result = &mut completion => result?,
                    signal = interrupt_signal() => {
                        signal?;
                        // The worker may have completed while JSONL was still
                        // backpressured. A late cancellation rejection must not
                        // discard its already-produced terminal event.
                        let _ = commands.send(WorkerCommand::root_cancel());
                        let _ = completion.await;
                        return Err(eyre!("interrupted"));
                    }
                }
            }
            Ok(())
        }
        .await;
        let shutdown_result = worker.shutdown().await;
        run_result?;
        shutdown_result
    }
}

async fn interrupt_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            signal = terminate.recv() => {
                if signal.is_none() {
                    return Err(eyre!("SIGTERM listener closed"));
                }
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}

pub(crate) async fn run_prompt(prompt: String, config: AgentArgs, vm: VmArgs) -> Result<()> {
    Run { prompt, repeat: 1 }.run(config, vm).await
}

async fn complete_turn(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<WorkerEvent>,
    output: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let mut terminal_emitted = false;
    let mut root_event_stream_closed = false;
    let mut finished = None;
    while let Some(event) = events.recv().await {
        match event {
            WorkerEvent::RootAgentEvent { event } => {
                let terminal = event.event.kind.is_terminal();
                write_turn_jsonl(&event.event, output).await?;
                if terminal {
                    terminal_emitted = true;
                    if let Some(result) = finished {
                        return result;
                    }
                }
            }
            WorkerEvent::TurnTraceRejected { .. } => {}
            WorkerEvent::TurnFinished {
                error: Some(error), ..
            } => return Err(eyre!(error)),
            WorkerEvent::TurnFinished { error: None, .. } => {
                if terminal_emitted {
                    return Ok(());
                }
                if root_event_stream_closed {
                    return Err(eyre!(
                        "root event stream closed before the turn emitted a terminal event"
                    ));
                }
                finished = Some(Ok(()));
            }
            WorkerEvent::RootEventStreamClosed => {
                root_event_stream_closed = true;
                if let Some(result) = finished {
                    result?;
                    return Err(eyre!(
                        "root event stream closed before the turn emitted a terminal event"
                    ));
                }
            }
            _ => {}
        }
    }
    if let Some(result) = finished {
        return result;
    }
    Err(eyre!("application worker stopped before the turn finished"))
}

async fn write_turn_jsonl(
    event: &nanocodex::agent::events::AgentEvent,
    output: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let mut record = serde_json::to_vec(event)?;
    record.push(b'\n');
    output.write_all(&record).await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use eyre::Result;
    use nanocodex::agent::events::{AgentEvent, AgentEventKind, AgentEventTiming, TimedAgentEvent};
    use serde_json::value::RawValue;
    use tokio::sync::mpsc;

    use super::{complete_turn, write_turn_jsonl};
    use crate::tui::WorkerEvent;

    #[tokio::test]
    async fn writes_root_terminal_event_as_jsonl() -> Result<()> {
        let event = AgentEvent {
            protocol_version: 1,
            request_id: Arc::from("session"),
            seq: 1,
            kind: AgentEventKind::RunCompleted,
            payload: RawValue::from_string(r#"{"status":"ok"}"#.to_owned())?.into(),
        };
        let mut output = Vec::new();

        write_turn_jsonl(&event, &mut output).await?;

        assert_eq!(
            String::from_utf8(output)?,
            "{\"protocol_version\":1,\"request_id\":\"session\",\"seq\":1,\"type\":\"run.completed\",\"payload\":{\"status\":\"ok\"}}\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn waits_for_a_terminal_root_event_after_worker_completion() -> Result<()> {
        let (updates, mut update_rx) = mpsc::unbounded_channel();
        updates.send(WorkerEvent::root_turn_finished(None))?;
        updates.send(WorkerEvent::RootAgentEvent {
            event: TimedAgentEvent {
                event: AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("session"),
                    seq: 1,
                    kind: AgentEventKind::RunCompleted,
                    payload: RawValue::from_string(r#"{"status":"ok"}"#.to_owned())?.into(),
                },
                timing: AgentEventTiming {
                    emitted_ns: 1,
                    source_received_ns: Some(1),
                },
            },
        })?;
        let mut output = Vec::new();

        complete_turn(&mut update_rx, &mut output).await?;

        assert_eq!(
            String::from_utf8(output)?,
            "{\"protocol_version\":1,\"request_id\":\"session\",\"seq\":1,\"type\":\"run.completed\",\"payload\":{\"status\":\"ok\"}}\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn waits_for_worker_completion_after_the_root_event_stream_closes() -> Result<()> {
        let (updates, mut update_rx) = mpsc::unbounded_channel();
        updates.send(WorkerEvent::RootAgentEvent {
            event: TimedAgentEvent {
                event: AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("session"),
                    seq: 1,
                    kind: AgentEventKind::RunCompleted,
                    payload: RawValue::from_string(r#"{"status":"ok"}"#.to_owned())?.into(),
                },
                timing: AgentEventTiming {
                    emitted_ns: 1,
                    source_received_ns: Some(1),
                },
            },
        })?;
        updates.send(WorkerEvent::RootEventStreamClosed)?;
        updates.send(WorkerEvent::root_turn_finished(None))?;
        let mut output = Vec::new();

        complete_turn(&mut update_rx, &mut output).await?;

        assert_eq!(
            String::from_utf8(output)?,
            "{\"protocol_version\":1,\"request_id\":\"session\",\"seq\":1,\"type\":\"run.completed\",\"payload\":{\"status\":\"ok\"}}\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_a_completed_turn_without_a_terminal_root_event() -> Result<()> {
        let (updates, mut update_rx) = mpsc::unbounded_channel();
        updates.send(WorkerEvent::root_turn_finished(None))?;
        updates.send(WorkerEvent::RootEventStreamClosed)?;
        let mut output = Vec::new();

        let error = complete_turn(&mut update_rx, &mut output)
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "root event stream closed before the turn emitted a terminal event"
        );
        Ok(())
    }
}
