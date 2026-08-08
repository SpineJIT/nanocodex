use std::{process::ExitCode, sync::Arc};

use clap::{Parser, builder::NonEmptyStringValueParser};
use eyre::{Result, eyre};
use nanocodex_spine_runtime::{
    SpineRuntime, SpineRuntimeError, SpineRuntimeLimits, SpineTreeObserver, with_spine_tools,
};
use tokio::sync::mpsc;

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
    runtime: Arc<SpineRuntime>,
}

impl SpineWorkerFactory {
    async fn build(config: AgentArgs, vm: VmArgs, runtime: Arc<SpineRuntime>) -> Result<Self> {
        let runtime_for_tools = Arc::clone(&runtime);
        let customizer: ToolCustomizer = Arc::new(move |tools, agent| {
            with_spine_tools(tools, agent, Arc::clone(&runtime_for_tools))
        });
        Ok(Self {
            configured: config.build_with_tool_customizer(vm, customizer).await?,
            runtime,
        })
    }
}

impl WorkerFactory for SpineWorkerFactory {
    type Command = WorkerCommand;
    type Event = WorkerEvent;

    fn start(self) -> WorkerHandle<Self::Command, Self::Event> {
        let runtime = self.runtime;
        start_configured_worker(self.configured, spine_capabilities(), move |updates| {
            if let Err(error) = bind_spine_tree_updates(&runtime, updates.clone()) {
                let _ = updates.send(WorkerEvent::SpineTreeFailed {
                    error: error.to_string(),
                });
            }
        })
    }
}

fn bind_spine_tree_updates(
    runtime: &SpineRuntime,
    updates: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<(), SpineRuntimeError> {
    let observer: SpineTreeObserver = Arc::new(move |snapshot| {
        let _ = updates.send(WorkerEvent::SpineTreeUpdated { snapshot });
    });
    runtime.set_tree_observer(observer)
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
    use std::sync::Arc;

    use clap::Parser;
    use nanocodex_spine_runtime::{SpineRuntime, SpineRuntimeLimits};
    use tokio::sync::mpsc;

    use super::{Cli, bind_spine_tree_updates, run};
    use crate::tui::WorkerEvent;

    #[test]
    fn spine_cli_reuses_the_standard_agent_flags() {
        let cli = Cli::try_parse_from(["nanocodex-spine", "--api-key", "test-key"]);

        assert!(cli.is_ok());
    }

    #[test]
    fn spine_cli_rejects_session_resume() {
        let cli = Cli::try_parse_from(["nanocodex-spine", "resume"]);

        assert!(cli.is_err());
    }

    #[tokio::test]
    async fn spine_cli_rejects_legacy_subagents_before_startup() {
        let cli = Cli::try_parse_from(["nanocodex-spine", "--subagents", "true"]).unwrap();

        let error = run(cli).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "nanocodex-spine does not support --subagents; use the synchronous Spine continuation tools instead"
        );
    }

    #[test]
    fn spine_tree_observer_forwards_snapshots_to_the_tui() {
        let runtime = Arc::new(SpineRuntime::new(SpineRuntimeLimits::default()));
        let (updates, mut received) = mpsc::unbounded_channel();

        bind_spine_tree_updates(&runtime, updates).unwrap();

        assert!(matches!(
            received.try_recv(),
            Ok(WorkerEvent::SpineTreeUpdated { snapshot }) if snapshot.active_node_id == "1"
        ));
    }
}
