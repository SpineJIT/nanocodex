use std::path::PathBuf;

use clap::Args;
use eyre::Result;

use crate::{benchmark, config::AgentArgs, observability::ObservabilityArgs, run, tui, vm::VmArgs};

#[derive(Args)]
pub(super) struct Benchmark {
    /// Evaluation profile. Uses nanocodex.toml's default when omitted.
    profile: Option<String>,

    /// Closed evaluation manifest.
    #[arg(long, default_value = "nanocodex.toml")]
    config: PathBuf,

    /// Durable SQLite ledger and retained artifacts.
    ///
    /// The workflow and child commands default to ~/.nanocodex/evals.
    #[arg(long, value_name = "DIRECTORY")]
    state_dir: Option<PathBuf>,

    /// Run the same benchmark workflow as flushed JSONL without a TUI.
    #[arg(long)]
    headless: bool,

    #[command(flatten)]
    agent: AgentArgs,

    #[command(flatten)]
    observability: ObservabilityArgs,

    #[command(flatten)]
    vm: VmArgs,
}

impl Benchmark {
    pub(super) async fn run(self) -> Result<()> {
        let prompt = benchmark::prompt(
            self.profile.as_deref(),
            &self.config,
            self.state_dir.as_deref(),
        );
        if self.headless {
            let _observability = self.observability.install(false, self.agent.cwd())?;
            run::run_prompt(prompt, self.agent, self.vm).await
        } else {
            let _observability = self.observability.install(true, self.agent.cwd())?;
            let display = self.profile.as_ref().map_or_else(
                || "/benchmark".to_owned(),
                |profile| format!("/benchmark {profile}"),
            );
            tui::run(
                self.agent,
                self.vm,
                Some(tui::InitialPrompt::workflow(display, prompt)),
                None,
            )
            .await
        }
    }
}
