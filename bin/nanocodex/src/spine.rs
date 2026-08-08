use std::{process::ExitCode, sync::Arc};

use clap::{Parser, builder::NonEmptyStringValueParser};
use eyre::{Result, eyre};
use nanocodex_spine_runtime::{SpineRuntime, SpineRuntimeLimits, with_spine_tools};

use crate::{
    app_core::{
        WorkerCapabilities, WorkerCapability, WorkerFactory, WorkerHandle, start_configured_worker,
    },
    config::{AgentArgs, ConfiguredAgent, ToolCustomizer},
    observability::ObservabilityArgs,
    tui::{self, InitialPrompt, WorkerCommand, WorkerEvent},
    vm::VmArgs,
};

/// Runs the experimental synchronous Spine continuation application.
pub fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            ExitCode::from(1)
        }
    }
}

fn try_main() -> Result<()> {
    nanocodex::oai::transport::install_default_rustls_crypto_provider();
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

#[derive(Parser)]
#[command(
    version = crate::version::SHORT_VERSION,
    long_version = crate::version::LONG_VERSION,
    about = "Experimental Nanocodex TUI with synchronous Spine continuations"
)]
struct Cli {
    #[command(flatten)]
    agent: AgentArgs,

    #[command(flatten)]
    observability: ObservabilityArgs,

    #[command(flatten)]
    vm: VmArgs,

    /// Submit an initial prompt immediately after the TUI opens.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    prompt: Option<String>,
}

async fn run(cli: Cli) -> Result<()> {
    if cli.agent.subagents_enabled() {
        return Err(eyre!(
            "nanocodex-spine does not support --subagents; use the synchronous Spine continuation tools instead"
        ));
    }
    let cwd = cli.agent.cwd().to_path_buf();
    let model = cli.agent.model();
    let thinking = cli.agent.thinking();
    let fast_mode = cli.agent.fast_mode();
    let _observability = cli.observability.install(true, &cwd)?;
    let runtime = Arc::new(SpineRuntime::new(SpineRuntimeLimits::default()));
    let factory = SpineWorkerFactory::build(cli.agent, cli.vm, runtime).await?;
    tui::run_with_worker(
        factory,
        cwd,
        model,
        thinking,
        fast_mode,
        Vec::new(),
        cli.prompt.map(InitialPrompt::plain),
    )
    .await
}

struct SpineWorkerFactory {
    configured: ConfiguredAgent,
}

impl SpineWorkerFactory {
    async fn build(config: AgentArgs, vm: VmArgs, runtime: Arc<SpineRuntime>) -> Result<Self> {
        let customizer: ToolCustomizer =
            Arc::new(move |tools, agent| with_spine_tools(tools, agent, Arc::clone(&runtime)));
        Ok(Self {
            configured: config.build_with_tool_customizer(vm, customizer).await?,
        })
    }
}

impl WorkerFactory for SpineWorkerFactory {
    type Command = WorkerCommand;
    type Event = WorkerEvent;

    fn start(self) -> WorkerHandle<Self::Command, Self::Event> {
        start_configured_worker(self.configured, spine_capabilities())
    }
}

const fn spine_capabilities() -> WorkerCapabilities {
    WorkerCapabilities::empty()
        .with(WorkerCapability::Prompt)
        .with(WorkerCapability::Steer)
        .with(WorkerCapability::Cancel)
        .with(WorkerCapability::FastMode)
        .with(WorkerCapability::Thinking)
        .with(WorkerCapability::Mcp)
        .with(WorkerCapability::Voice)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn spine_cli_reuses_the_standard_agent_flags() {
        let cli = Cli::try_parse_from(["nanocodex-spine", "--api-key", "test-key"]);

        assert!(cli.is_ok());
    }
}
