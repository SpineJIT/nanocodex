use std::{path::PathBuf, process::ExitCode, sync::Arc};

use clap::{Parser, Subcommand, builder::NonEmptyStringValueParser};
use eyre::{Result, eyre};
use nanocodex::agent::rollout::RolloutTranscriptItem;
use nanocodex_spine_runtime::{SpineAbortReason, SpineDelivery, SpineRuntime, SpineRuntimeLimits};
use tokio::sync::mpsc;

use crate::{
    app_core::{WorkerFactory, WorkerHandle},
    config::{AgentArgs, ConfiguredAgent, default_codex_home},
    observability::ObservabilityArgs,
    spine_worker::{
        SpineIntentChannel, SpineSessionRecipe, SpineWorker, SpineWorkerInitial, capabilities,
        durable_prompt_cache_key,
    },
    tui::{self, InitialPrompt, RestoredTranscript, WorkerCommand, WorkerEvent},
    vm::VmArgs,
};

/// Runs the experimental durable Spine current-node application.
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
    about = "Experimental Nanocodex TUI with durable Spine continuations"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

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

#[derive(Subcommand)]
enum Command {
    /// Restore the active node of one durable Spine root session.
    Resume {
        /// Spine root UUID recorded in the sidecar journal filename.
        root_thread_id: String,
    },
}

async fn run(mut cli: Cli) -> Result<()> {
    cli.agent.enable_subagents_by_default();
    if !cli.agent.rollouts_enabled() {
        return Err(eyre!(
            "nanocodex-spine requires standard rollout recording; remove --rollouts false"
        ));
    }
    if cli.command.is_some() && cli.prompt.is_some() {
        return Err(eyre!("nanocodex-spine resume does not accept --prompt"));
    }
    let observability_cwd = cli.agent.cwd().to_path_buf();
    let _observability = cli.observability.install(true, &observability_cwd)?;
    let (factory, initial_prompt) = match cli.command {
        Some(Command::Resume { root_thread_id }) => {
            let factory = SpineWorkerFactory::resume(cli.agent, cli.vm, &root_thread_id).await?;
            let initial_prompt = factory
                .manual_delivery
                .as_ref()
                .map(|delivery| factory.runtime.delivery_prompt(delivery))
                .transpose()?
                .map(InitialPrompt::prefill);
            (factory, initial_prompt)
        }
        None => (
            SpineWorkerFactory::build(cli.agent, cli.vm).await?,
            cli.prompt.map(InitialPrompt::plain),
        ),
    };
    eprintln!(
        "Spine root: {}; resume with: nanocodex-spine resume {}",
        factory.root_session_id, factory.root_session_id
    );
    let initial = factory.initial.clone();
    tui::run_with_worker(
        factory,
        initial.cwd,
        initial.model,
        initial.thinking,
        initial.fast_mode,
        RestoredTranscript::spine(initial.transcript, initial.spine_delivery_ids),
        initial_prompt,
    )
    .await
}

struct SpineWorkerFactory {
    configured: ConfiguredAgent,
    runtime: Arc<SpineRuntime>,
    intents: mpsc::UnboundedReceiver<crate::spine_worker::IntentCommand>,
    session_recipe: SpineSessionRecipe,
    root_session_id: String,
    active_session_id: String,
    initial_delivery: Option<SpineDelivery>,
    manual_delivery: Option<SpineDelivery>,
    initial_status: Option<String>,
    initial: SpineInitial,
}

#[derive(Clone)]
struct SpineInitial {
    cwd: PathBuf,
    model: nanocodex::Model,
    thinking: nanocodex::Thinking,
    fast_mode: bool,
    transcript: Vec<RolloutTranscriptItem>,
    spine_delivery_ids: std::collections::BTreeSet<String>,
}

impl SpineWorkerFactory {
    async fn build(config: AgentArgs, vm: VmArgs) -> Result<Self> {
        let codex_home = default_codex_home()?;
        Self::build_in(config, vm, codex_home).await
    }

    async fn build_in(config: AgentArgs, vm: VmArgs, codex_home: PathBuf) -> Result<Self> {
        let cwd = config.cwd().to_path_buf();
        let initial = SpineInitial {
            cwd,
            model: config.model(),
            thinking: config.thinking(),
            fast_mode: config.fast_mode(),
            transcript: Vec::new(),
            spine_delivery_ids: std::collections::BTreeSet::new(),
        };
        let (intent_sink, intents) = SpineIntentChannel::new();
        let session_recipe = SpineSessionRecipe::new(config, vm, intent_sink, codex_home.clone());
        let configured = session_recipe.build_root().await?;
        let root_session_id = configured.handle.session_id().to_string();
        let journal_directory = codex_home.join("spine");
        let runtime = Arc::new(
            SpineRuntime::create(
                SpineRuntimeLimits::default(),
                &journal_directory,
                &root_session_id,
                &root_session_id,
                chrono::Utc::now().to_rfc3339(),
            )
            .map_err(|error| eyre!(error))?,
        );
        Ok(Self {
            configured,
            runtime,
            intents,
            session_recipe,
            root_session_id: root_session_id.clone(),
            active_session_id: root_session_id,
            initial_delivery: None,
            manual_delivery: None,
            initial_status: None,
            initial,
        })
    }

    async fn resume(config: AgentArgs, vm: VmArgs, root_thread_id: &str) -> Result<Self> {
        let codex_home = default_codex_home()?;
        Self::resume_in(config, vm, root_thread_id, codex_home).await
    }

    async fn resume_in(
        config: AgentArgs,
        vm: VmArgs,
        root_thread_id: &str,
        codex_home: PathBuf,
    ) -> Result<Self> {
        uuid::Uuid::parse_str(root_thread_id)
            .map_err(|error| eyre!("invalid Spine root thread ID `{root_thread_id}`: {error}"))?;
        let journal_directory = codex_home.join("spine");
        let runtime = Arc::new(
            SpineRuntime::open(
                SpineRuntimeLimits::default(),
                &journal_directory,
                root_thread_id,
            )
            .map_err(|error| eyre!(error))?,
        );
        let recovered_pending = runtime.pending_transition().map_err(|error| eyre!(error))?;
        if let Some(pending) = &recovered_pending {
            runtime
                .abort_prepared(
                    pending,
                    SpineAbortReason::CoordinatorStoppedBeforeCommit,
                    Some(format!("recovery-{}", uuid::Uuid::now_v7())),
                )
                .map_err(|error| eyre!(error))?;
        }
        let root_session_id = runtime.root_session_id().map_err(|error| eyre!(error))?;
        let active_session_id = runtime.active_session_id().map_err(|error| eyre!(error))?;
        let thinking = config.thinking();
        let fast_mode = config.fast_mode();
        let (intent_sink, intents) = SpineIntentChannel::new();
        let session_recipe = SpineSessionRecipe::new(config, vm, intent_sink, codex_home);
        let projection = runtime.projection().map_err(|error| eyre!(error))?;
        let durable = match session_recipe.load(&active_session_id) {
            Ok(durable) => durable,
            Err(error) if root_session_id == active_session_id && projection.nodes.len() == 1 => {
                return Err(error.wrap_err(format!(
                    "Spine root {root_session_id} has no durable Nanocodex boundary yet; \
                     resume is available after the first completed Spine transition"
                )));
            }
            Err(error) => return Err(error),
        };
        let cache_key = durable_prompt_cache_key(&durable)?;
        if cache_key != runtime.prompt_cache_key().map_err(|error| eyre!(error))? {
            return Err(eyre!(
                "active Spine rollout prompt cache key does not match the root journal"
            ));
        }
        let initial = SpineInitial {
            cwd: PathBuf::from(durable.workspace()),
            model: durable.model(),
            thinking,
            fast_mode,
            transcript: durable.transcript().to_vec(),
            spine_delivery_ids: runtime
                .active_delivery_ids()
                .map_err(|error| eyre!(error))?,
        };
        let configured = session_recipe.build_resumed(durable).await?;
        if configured.handle.session_id().to_string() != active_session_id {
            return Err(eyre!(
                "resumed Spine session ID does not match the journal active session"
            ));
        }
        let initial_delivery = runtime
            .unclaimed_active_delivery()
            .map_err(|error| eyre!(error))?;
        let manual_delivery = if initial_delivery.is_some() {
            None
        } else {
            runtime
                .claimed_active_delivery()
                .map_err(|error| eyre!(error))?
        };
        let initial_status = manual_delivery.as_ref().map(|_| {
            "Spine recovery needs confirmation: the previous continuation was claimed but not accepted. \
             Review the prefilled prompt and submit it to continue."
                .to_owned()
        }).or_else(|| {
            recovered_pending.map(|_| {
                "Recovered an uncommitted Spine transition; continuing from the last durable node."
                    .to_owned()
            })
        });
        Ok(Self {
            configured,
            runtime,
            intents,
            session_recipe,
            root_session_id,
            active_session_id,
            initial_delivery,
            manual_delivery,
            initial_status,
            initial,
        })
    }
}

impl WorkerFactory for SpineWorkerFactory {
    type Command = WorkerCommand;
    type Event = WorkerEvent;

    fn start(self) -> WorkerHandle<Self::Command, Self::Event> {
        SpineWorker::start(
            self.configured,
            self.runtime,
            self.intents,
            self.session_recipe,
            SpineWorkerInitial {
                initial_delivery: self.initial_delivery,
                manual_delivery: self.manual_delivery,
                initial_status: self.initial_status,
                root_session_id: self.root_session_id,
                active_session_id: self.active_session_id,
                capabilities: capabilities(),
            },
        )
    }
}

#[cfg(test)]
#[path = "spine_tests.rs"]
mod tests;
