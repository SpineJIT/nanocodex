use clap::{Args, builder::NonEmptyStringValueParser};
use eyre::{Result, eyre};
use nanocodex::AgentEvents;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::config::AgentArgs;

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
    pub(crate) async fn run(self, config: AgentArgs) -> Result<()> {
        let configured = config.build().await?;
        let handle = configured.handle;
        let mut events = configured.events;
        let mut stdout = tokio::io::stdout();
        let run_result: Result<()> = async {
            for _ in 0..self.repeat {
                let turn = handle.prompt(self.prompt.clone()).await?;
                let control = turn.control();
                let completion = async {
                    write_turn_jsonl(&mut events, &mut stdout).await?;
                    turn.result().await?;
                    Ok::<(), eyre::Report>(())
                };
                tokio::pin!(completion);
                tokio::select! {
                    result = &mut completion => result?,
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        // The driver may have completed while JSONL was still
                        // backpressured. A late cancellation rejection must not
                        // discard its already-produced terminal event.
                        let _ = control.cancel().await;
                        let _ = completion.await;
                        return Err(eyre!("interrupted"));
                    }
                }
                handle.flush_rollout().await?;
            }
            Ok(())
        }
        .await;
        let agent_shutdown = handle.shutdown().await;
        drop(handle);
        drop(events);
        if let Some(child_agents) = configured.child_agents {
            child_agents.shutdown().await;
        }
        let shutdown_result = if let Some(adapter) = configured.mpp_adapter {
            adapter.shutdown().await
        } else {
            Ok(())
        };
        run_result?;
        agent_shutdown?;
        shutdown_result
    }
}

async fn write_turn_jsonl(
    events: &mut AgentEvents,
    output: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    while let Some(event) = events.recv().await {
        let terminal = event.kind.is_terminal();
        let mut record = serde_json::to_vec(&event)?;
        record.push(b'\n');
        output.write_all(&record).await?;
        output.flush().await?;
        if terminal {
            return Ok(());
        }
    }
    Err(eyre!(
        "agent event stream closed before the turn emitted a terminal event"
    ))
}
