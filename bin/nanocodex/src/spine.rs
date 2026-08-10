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
mod tests {
    use std::{
        collections::BTreeSet,
        future::{Ready, ready},
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };

    use clap::Parser;
    use eyre::eyre;
    use futures_util::{SinkExt, StreamExt};
    use nanocodex::{
        Nanocodex, OpenAi, Thinking, Tools,
        agent::{rollout::RolloutConfig, session::SessionId},
        oai::{
            ResponseError,
            responses::{ContentItem, MessageRole, ResponseItem, Usage, WarmupResponse},
            tower::{
                CodeCall, CodeCallKind, GenerationOutput, ResponsePipelineStats, ResponsesAttempt,
                ResponsesAttemptKind, ResponsesOutput, ResponsesServiceResponse,
            },
        },
    };
    use nanocodex_spine_runtime::{
        SpineDeliveryStatus, SpineIntentRequest, SpineIntentSink, SpineRuntime, SpineRuntimeLimits,
        with_spine_tools,
    };
    use tempfile::tempdir;
    use tokio::{net::TcpListener, sync::Notify, time::timeout};
    use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};
    use tower::Service;

    use super::{Cli, SpineWorkerFactory, run};
    use crate::{
        app_core::WorkerFactory,
        config::ConfiguredAgent,
        spine_worker::{
            DeliveryFault, SpineIntentChannel, SpineSessionRecipe, SpineWorker, SpineWorkerInitial,
            TestResumedAgentBuilder, capabilities, durable_prompt_cache_key,
        },
        subagents::{self, ChildAgents},
        tui::{PaneId, WorkerCommand, WorkerEvent},
    };

    #[test]
    fn spine_cli_reuses_the_standard_agent_flags() {
        let cli = Cli::try_parse_from(["nanocodex-spine", "--api-key", "test-key"]);

        assert!(cli.is_ok());
    }

    #[test]
    fn spine_cli_accepts_an_explicit_session_resume() {
        let cli = Cli::try_parse_from([
            "nanocodex-spine",
            "resume",
            "019c0d31-c308-7d91-bff4-5dca82d15ac6",
        ]);

        assert!(cli.is_ok());
    }

    #[test]
    fn spine_cli_enables_subagents_for_the_active_node_by_default() {
        let mut cli = Cli::try_parse_from(["nanocodex-spine"])
            .expect("Spine CLI accepts its default subagent policy");
        cli.agent.enable_subagents_by_default();

        assert!(cli.agent.subagents_enabled());
    }

    #[test]
    fn spine_cli_respects_an_explicit_subagent_opt_out() {
        let mut cli = Cli::try_parse_from(["nanocodex-spine", "--subagents", "false"])
            .expect("Spine CLI accepts an explicit subagent policy");
        cli.agent.enable_subagents_by_default();

        assert!(!cli.agent.subagents_enabled());
    }

    #[tokio::test]
    async fn spine_cli_requires_standard_rollout_recording() {
        let cli = Cli::try_parse_from([
            "nanocodex-spine",
            "--subagents",
            "false",
            "--rollouts",
            "false",
        ])
        .unwrap();

        let error = run(cli).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "nanocodex-spine requires standard rollout recording; remove --rollouts false"
        );
    }

    #[tokio::test]
    async fn terminal_open_next_and_close_switch_the_durable_current_node() -> eyre::Result<()> {
        let directory = tempdir()?;
        let root_session_id = SessionId::new();
        let calls = Arc::new(AtomicU32::new(0));
        let parent_finished = Arc::new(Notify::new());
        let openai = OpenAi::builder("test-key")
            .service({
                let calls = Arc::clone(&calls);
                let parent_finished = Arc::clone(&parent_finished);
                move || TerminalSpineService {
                    calls: Arc::clone(&calls),
                    parent_finished: Arc::clone(&parent_finished),
                }
            })
            .build()?;
        let tools = Tools::builder().without_defaults().build()?;
        let (intent_sink, intents) = SpineIntentChannel::new();
        let tool_sink: Arc<dyn SpineIntentSink> = intent_sink.clone();
        let (agent, events) = Nanocodex::builder(openai)
            .thinking(Thinking::Low)
            .workspace(directory.path())
            .session_id(root_session_id)
            .rollout(RolloutConfig::new(directory.path()))
            .tools_factory(move |_agent| with_spine_tools(tools.clone(), Arc::clone(&tool_sink)))
            .build()?;
        let root_session_id = agent.session_id().to_string();
        let runtime = Arc::new(SpineRuntime::create(
            SpineRuntimeLimits::default(),
            directory.path().join("spine").as_path(),
            &root_session_id,
            &root_session_id,
            "2026-08-09T00:00:00Z",
        )?);
        let cli = Cli::try_parse_from([
            "nanocodex-spine",
            "--api-key",
            "test-key",
            "--browser=none",
            "--subagents",
            "false",
        ])?;
        let session_recipe = SpineSessionRecipe::new(
            cli.agent,
            cli.vm,
            Arc::clone(&intent_sink),
            directory.path().to_path_buf(),
        );
        let resumed_recipe = session_recipe.clone();
        let configured = ConfiguredAgent {
            handle: agent,
            events,
            realtime: None,
            child_agents: None,
            mpp_adapter: None,
            mcp: None,
            browser: None,
            vm: None,
        };
        let mut worker = SpineWorker::start(
            configured,
            Arc::clone(&runtime),
            intents,
            session_recipe,
            SpineWorkerInitial {
                initial_delivery: None,
                manual_delivery: None,
                initial_status: None,
                root_session_id: root_session_id.clone(),
                active_session_id: root_session_id.clone(),
                capabilities: capabilities(),
            },
        );

        worker.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 1,
            prompt: "inspect the parser".into(),
        })?;
        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    worker.events_mut().recv().await,
                    Some(WorkerEvent::TurnFinished {
                        target: PaneId::Main,
                        main_branch_id: Some(0),
                        error: None,
                    })
                ) {
                    return;
                }
            }
        })
        .await?;
        timeout(Duration::from_secs(5), parent_finished.notified()).await?;

        let projection = runtime.projection()?;
        assert_eq!(runtime.active_session_id()?, root_session_id);
        assert_eq!(projection.cursor.to_string(), "1");
        assert_eq!(projection.nodes.len(), 3);
        assert_eq!(
            projection.nodes[1].summary.as_deref(),
            Some("inspect the parser")
        );
        assert_eq!(
            projection.nodes[2].summary.as_deref(),
            Some("write a parser fix")
        );
        assert_eq!(calls.load(Ordering::Relaxed), 5);

        worker.shutdown().await?;
        let durable = resumed_recipe.load(&root_session_id)?;
        assert_eq!(durable_prompt_cache_key(&durable)?, root_session_id);
        let restored_parent = resumed_recipe.build_resumed(durable).await?;
        assert_eq!(
            restored_parent.handle.session_id().to_string(),
            root_session_id
        );
        restored_parent.handle.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn delivery_failures_after_commit_stop_the_spine_coordinator() -> eyre::Result<()> {
        for (fault, expected_claim) in [
            (DeliveryFault::Claim, false),
            (DeliveryFault::PromptAcceptance, true),
            (DeliveryFault::AcceptedSync, true),
        ] {
            let directory = tempdir()?;
            let (worker, runtime) =
                scripted_spine_worker(directory.path(), Some(fault), None).await?;
            worker.commands().send(WorkerCommand::Prompt {
                target: PaneId::Main,
                prompt_id: 1,
                prompt: "open a child scope".into(),
            })?;

            timeout(Duration::from_secs(5), async {
                loop {
                    if runtime.active_session_id()? != runtime.root_session_id()? {
                        return Ok::<(), eyre::Report>(());
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;

            let outcome = timeout(Duration::from_secs(5), worker.shutdown()).await?;
            assert!(outcome.is_err(), "{fault:?} must stop the coordinator");
            assert!(
                runtime.active_session_id()? != runtime.root_session_id()?,
                "{fault:?} must occur after the transition commit"
            );
            if expected_claim {
                assert!(runtime.claimed_active_delivery()?.is_some());
            } else {
                assert!(runtime.unclaimed_active_delivery()?.is_some());
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn manual_delivery_confirmation_is_accepted_once() -> eyre::Result<()> {
        let directory = tempdir()?;
        let (worker, runtime, delivery) = claimed_delivery_worker(directory.path(), None).await?;

        worker.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 1,
            prompt: "continue with the reviewed handoff".into(),
        })?;
        timeout(Duration::from_secs(5), async {
            loop {
                if runtime.delivery_status(delivery.id())? == Some(SpineDeliveryStatus::Accepted) {
                    return Ok::<(), eyre::Report>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        assert!(runtime.claimed_active_delivery()?.is_none());
        assert!(runtime.unclaimed_active_delivery()?.is_none());

        worker.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn manual_delivery_accept_sync_failure_stops_the_spine_coordinator() -> eyre::Result<()> {
        let directory = tempdir()?;
        let (worker, runtime, delivery) =
            claimed_delivery_worker(directory.path(), Some(DeliveryFault::AcceptedSync)).await?;

        worker.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 1,
            prompt: "confirm the interrupted handoff".into(),
        })?;

        let outcome = timeout(Duration::from_secs(5), worker.shutdown()).await?;
        assert!(
            outcome.is_err(),
            "manual accepted-sync failure must stop the worker"
        );
        assert_eq!(
            runtime.delivery_status(delivery.id())?,
            Some(SpineDeliveryStatus::Claimed)
        );
        Ok(())
    }

    #[tokio::test]
    async fn factory_resume_restores_identity_and_delivers_unclaimed_continuation()
    -> eyre::Result<()> {
        let directory = tempdir()?;
        let (endpoint, server) = completed_responses_server(2).await?;
        let cli = spine_cli_with_websocket(&endpoint)?;
        let factory = SpineWorkerFactory::build_in(
            cli.agent.clone(),
            cli.vm.clone(),
            directory.path().to_path_buf(),
        )
        .await?;
        let root_session_id = factory.root_session_id.clone();
        let mut root = factory.start();
        root.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 1,
            prompt: "establish a durable root boundary".into(),
        })?;
        wait_for_main_turn(&mut root).await?;
        root.shutdown().await?;
        let durable_root = RolloutConfig::new(directory.path()).load_session(&root_session_id)?;
        assert_eq!(durable_root.thread_id(), root_session_id);

        let factory = SpineWorkerFactory::resume_in(
            cli.agent.clone(),
            cli.vm.clone(),
            &root_session_id,
            directory.path().to_path_buf(),
        )
        .await?;
        let (child, _events) = factory.configured.handle.fork().await?;
        let active_session_id = child.session_id().to_string();
        child.shutdown().await?;
        let runtime = Arc::clone(&factory.runtime);
        drop(factory);
        let transition = runtime.prepare(SpineIntentRequest::new(
            root_session_id.clone(),
            "resume-delivery",
            nanocodex_spine_runtime::SpineTerminalControl::Open {
                summary: "inspect the recovered scope".to_owned(),
            },
        ))?;
        let delivery = runtime.commit(
            &transition,
            active_session_id.clone(),
            None,
            "delivery-resume-unclaimed",
        )?;
        drop(runtime);

        let factory = SpineWorkerFactory::resume_in(
            cli.agent,
            cli.vm,
            &root_session_id,
            directory.path().to_path_buf(),
        )
        .await?;
        assert_eq!(
            factory.configured.handle.session_id().to_string(),
            active_session_id
        );
        assert_eq!(factory.active_session_id, active_session_id);
        assert_eq!(
            durable_prompt_cache_key(&factory.session_recipe.load(&active_session_id)?)?,
            factory.runtime.prompt_cache_key()?
        );
        assert_eq!(factory.initial_delivery.as_ref(), Some(&delivery));

        let runtime = Arc::clone(&factory.runtime);
        let resumed = factory.start();
        timeout(Duration::from_secs(5), async {
            loop {
                if runtime.delivery_status(delivery.id())? == Some(SpineDeliveryStatus::Accepted) {
                    return Ok::<(), eyre::Report>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        resumed.shutdown().await?;
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn factory_resume_accepts_a_claimed_manual_delivery_without_replaying_it()
    -> eyre::Result<()> {
        let directory = tempdir()?;
        let (endpoint, server) = completed_responses_server(2).await?;
        let cli = spine_cli_with_websocket(&endpoint)?;
        let factory = SpineWorkerFactory::build_in(
            cli.agent.clone(),
            cli.vm.clone(),
            directory.path().to_path_buf(),
        )
        .await?;
        let root_session_id = factory.root_session_id.clone();
        let mut root = factory.start();
        root.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 1,
            prompt: "establish a durable root boundary".into(),
        })?;
        wait_for_main_turn(&mut root).await?;
        root.shutdown().await?;

        let factory = SpineWorkerFactory::resume_in(
            cli.agent.clone(),
            cli.vm.clone(),
            &root_session_id,
            directory.path().to_path_buf(),
        )
        .await?;
        let (child, _events) = factory.configured.handle.fork().await?;
        let active_session_id = child.session_id().to_string();
        child.shutdown().await?;
        let runtime = Arc::clone(&factory.runtime);
        drop(factory);
        let transition = runtime.prepare(SpineIntentRequest::new(
            root_session_id.clone(),
            "manual-resume-delivery",
            nanocodex_spine_runtime::SpineTerminalControl::Open {
                summary: "review the interrupted work".to_owned(),
            },
        ))?;
        let delivery = runtime.commit(
            &transition,
            active_session_id.clone(),
            None,
            "delivery-resume-claimed",
        )?;
        runtime.claim_delivery(&delivery)?;
        drop(runtime);

        let factory = SpineWorkerFactory::resume_in(
            cli.agent.clone(),
            cli.vm.clone(),
            &root_session_id,
            directory.path().to_path_buf(),
        )
        .await?;
        assert!(factory.initial_delivery.is_none());
        assert_eq!(factory.manual_delivery.as_ref(), Some(&delivery));
        let runtime = Arc::clone(&factory.runtime);
        let resumed = factory.start();
        assert_eq!(
            runtime.delivery_status(delivery.id())?,
            Some(SpineDeliveryStatus::Claimed)
        );
        resumed.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 1,
            prompt: "continue after reviewing the handoff".into(),
        })?;
        timeout(Duration::from_secs(5), async {
            loop {
                if runtime.delivery_status(delivery.id())? == Some(SpineDeliveryStatus::Accepted) {
                    return Ok::<(), eyre::Report>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        resumed.shutdown().await?;
        drop(runtime);

        let factory = SpineWorkerFactory::resume_in(
            cli.agent,
            cli.vm,
            &root_session_id,
            directory.path().to_path_buf(),
        )
        .await?;
        assert!(factory.initial_delivery.is_none());
        assert!(factory.manual_delivery.is_none());
        factory.configured.handle.shutdown().await?;
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn factory_resume_rejects_missing_rollouts_and_cache_mismatches() -> eyre::Result<()> {
        let missing = tempdir()?;
        let missing_root = SessionId::new().to_string();
        let runtime = SpineRuntime::create(
            SpineRuntimeLimits::default(),
            missing.path().join("spine").as_path(),
            &missing_root,
            &missing_root,
            "2026-08-10T00:00:00Z",
        )?;
        drop(runtime);
        let missing_cli = spine_cli_with_websocket("ws://127.0.0.1:1")?;
        let error = match SpineWorkerFactory::resume_in(
            missing_cli.agent,
            missing_cli.vm,
            &missing_root,
            missing.path().to_path_buf(),
        )
        .await
        {
            Ok(_) => return Err(eyre!("a Spine journal without its active rollout resumed")),
            Err(error) => error,
        };
        assert!(error.to_string().contains("no durable Nanocodex boundary"));

        let mismatched = tempdir()?;
        let mismatched_root = SessionId::new();
        let openai = OpenAi::builder("test-key")
            .service(|| CompletedSpineService)
            .build()?;
        let (agent, _events) = Nanocodex::builder(openai)
            .session_id(mismatched_root)
            .rollout(RolloutConfig::new(mismatched.path()))
            .build()?;
        let root_session_id = agent.session_id().to_string();
        let turn = agent.prompt("persist a durable root boundary").await?;
        turn.result().await?;
        agent.flush_rollout().await?;
        agent.shutdown().await?;
        let runtime = SpineRuntime::create(
            SpineRuntimeLimits::default(),
            mismatched.path().join("spine").as_path(),
            &root_session_id,
            "unexpected-cache-key",
            "2026-08-10T00:00:00Z",
        )?;
        drop(runtime);
        let mismatched_cli = spine_cli_with_websocket("ws://127.0.0.1:1")?;
        let error = match SpineWorkerFactory::resume_in(
            mismatched_cli.agent,
            mismatched_cli.vm,
            &root_session_id,
            mismatched.path().to_path_buf(),
        )
        .await
        {
            Ok(_) => return Err(eyre!("a rollout with another cache key resumed")),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("active Spine rollout prompt cache key does not match the root journal")
        );
        Ok(())
    }

    #[tokio::test]
    async fn spine_tools_remain_visible_to_coordinator_forks_but_not_ordinary_children()
    -> eyre::Result<()> {
        let directory = tempdir()?;
        let profiles = Arc::new(Mutex::new(Vec::new()));
        let openai = OpenAi::builder("test-key")
            .service({
                let profiles = Arc::clone(&profiles);
                move || ToolProfileService {
                    profiles: Arc::clone(&profiles),
                }
            })
            .build()?;
        let base_tools = Tools::builder().without_defaults().build()?;
        let child_registry = Arc::new(ChildAgents::default());
        let primary_agents = Arc::downgrade(&child_registry);
        let child_agents = Arc::downgrade(&child_registry);
        let (intent_sink, _intents) = SpineIntentChannel::new();
        let tool_sink: Arc<dyn SpineIntentSink> = intent_sink;
        let root_handle = Arc::new(Mutex::new(None));
        let (root, _events) = Nanocodex::builder(openai)
            .rollout(RolloutConfig::new(directory.path()))
            .tools_factory({
                let base_tools = base_tools.clone();
                let tool_sink = Arc::clone(&tool_sink);
                let root_handle = Arc::clone(&root_handle);
                move |agent| {
                    let mut slot = root_handle.lock().expect("root tool handle lock");
                    if slot.is_none() {
                        *slot = Some(agent.clone());
                    }
                    drop(slot);
                    let tools = subagents::with_subagents(
                        base_tools.clone(),
                        agent,
                        primary_agents.clone(),
                    )?;
                    with_spine_tools(tools, Arc::clone(&tool_sink))
                }
            })
            .child_tools_factory({
                let base_tools = base_tools.clone();
                move |agent| {
                    subagents::with_subagents(base_tools.clone(), agent, child_agents.clone())
                }
            })
            .build()?;
        let root_handle = root_handle
            .lock()
            .expect("root tool handle lock")
            .clone()
            .expect("primary tools receive the root handle");

        root.prompt("establish a coordinator boundary")
            .await?
            .result()
            .await?;
        let (coordinator, _events) = root.fork().await?;
        coordinator
            .prompt("continue as the Spine coordinator")
            .await?
            .result()
            .await?;
        let (ordinary_fork, _events) = root_handle.fork().await?;
        ordinary_fork
            .prompt("inspect without becoming a coordinator")
            .await?
            .result()
            .await?;
        let (ordinary_spawn, _events) = root_handle.spawn().await?;
        ordinary_spawn
            .prompt("start as an ordinary child")
            .await?
            .result()
            .await?;

        {
            let profiles = profiles.lock().expect("tool profile lock");
            assert_eq!(profiles.len(), 4);
            for profile in &profiles[..2] {
                assert_spine_coordinator_tools(profile);
            }
            for profile in &profiles[2..] {
                assert_ordinary_child_tools(profile);
            }
        }

        coordinator.shutdown().await?;
        ordinary_fork.shutdown().await?;
        ordinary_spawn.shutdown().await?;
        root.shutdown().await?;
        child_registry.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn prompt_submitted_during_a_transition_is_rejected_with_its_contents() -> eyre::Result<()>
    {
        let directory = tempdir()?;
        let (started, started_receiver) = tokio::sync::oneshot::channel();
        let release = Arc::new(Notify::new());
        let (mut worker, _runtime) = scripted_spine_worker(
            directory.path(),
            None,
            Some((started, Arc::clone(&release))),
        )
        .await?;
        worker.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 1,
            prompt: "open a child scope".into(),
        })?;
        timeout(Duration::from_secs(5), started_receiver).await??;

        worker.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 2,
            prompt: "keep this exact draft".into(),
        })?;
        timeout(Duration::from_secs(5), async {
            loop {
                match worker.events_mut().recv().await {
                    Some(WorkerEvent::PromptRejected {
                        target: PaneId::Main,
                        prompt_id: 2,
                        prompt,
                        error,
                    }) => {
                        assert_eq!(prompt.display(), "keep this exact draft");
                        assert!(error.contains("transition is in progress"));
                        return;
                    }
                    Some(_) => {}
                    None => panic!("Spine worker stopped before rejecting the queued prompt"),
                }
            }
        })
        .await?;
        release.notify_one();
        timeout(Duration::from_secs(5), async {
            loop {
                match worker.events_mut().recv().await {
                    Some(WorkerEvent::SpineContinuation {
                        delivery_id,
                        prompt,
                    }) => {
                        assert!(delivery_id.starts_with("delivery-"));
                        assert!(prompt.starts_with("<spine_delivery id=\""));
                        return;
                    }
                    Some(_) => {}
                    None => panic!("Spine worker stopped before reporting its continuation"),
                }
            }
        })
        .await?;
        worker.shutdown().await?;
        Ok(())
    }

    async fn scripted_spine_worker(
        directory: &std::path::Path,
        delivery_fault: Option<DeliveryFault>,
        transition_gate: Option<(tokio::sync::oneshot::Sender<()>, Arc<Notify>)>,
    ) -> eyre::Result<(
        crate::app_core::WorkerHandle<WorkerCommand, WorkerEvent>,
        Arc<SpineRuntime>,
    )> {
        let root_session_id = SessionId::new();
        let openai = OpenAi::builder("test-key")
            .service(DeliveryFaultService::default)
            .build()?;
        let tools = Tools::builder().without_defaults().build()?;
        let (intent_sink, intents) = SpineIntentChannel::new();
        let tool_sink: Arc<dyn SpineIntentSink> = intent_sink.clone();
        let (agent, events) = Nanocodex::builder(openai)
            .thinking(Thinking::Low)
            .workspace(directory)
            .session_id(root_session_id)
            .rollout(RolloutConfig::new(directory))
            .tools_factory(move |_agent| with_spine_tools(tools.clone(), Arc::clone(&tool_sink)))
            .build()?;
        let root_session_id = agent.session_id().to_string();
        let runtime = Arc::new(SpineRuntime::create(
            SpineRuntimeLimits::default(),
            directory.join("spine").as_path(),
            &root_session_id,
            &root_session_id,
            "2026-08-09T00:00:00Z",
        )?);
        let cli = Cli::try_parse_from([
            "nanocodex-spine",
            "--api-key",
            "test-key",
            "--browser=none",
            "--subagents",
            "false",
        ])?;
        let session_recipe = SpineSessionRecipe::new(
            cli.agent,
            cli.vm,
            Arc::clone(&intent_sink),
            directory.to_path_buf(),
        );
        let configured = ConfiguredAgent {
            handle: agent,
            events,
            realtime: None,
            child_agents: None,
            mpp_adapter: None,
            mcp: None,
            browser: None,
            vm: None,
        };
        let initial = SpineWorkerInitial {
            initial_delivery: None,
            manual_delivery: None,
            initial_status: None,
            root_session_id: root_session_id.clone(),
            active_session_id: root_session_id,
            capabilities: capabilities(),
        };
        let worker = match (delivery_fault, transition_gate) {
            (Some(delivery_fault), None) => SpineWorker::start_with_delivery_fault(
                configured,
                Arc::clone(&runtime),
                intents,
                session_recipe,
                initial,
                delivery_fault,
            ),
            (None, Some((started, release))) => SpineWorker::start_with_transition_gate(
                configured,
                Arc::clone(&runtime),
                intents,
                session_recipe,
                initial,
                started,
                release,
            ),
            _ => {
                return Err(eyre!(
                    "scripted Spine worker needs exactly one test control"
                ));
            }
        };
        Ok((worker, runtime))
    }

    async fn claimed_delivery_worker(
        directory: &std::path::Path,
        delivery_fault: Option<DeliveryFault>,
    ) -> eyre::Result<(
        crate::app_core::WorkerHandle<WorkerCommand, WorkerEvent>,
        Arc<SpineRuntime>,
        nanocodex_spine_runtime::SpineDelivery,
    )> {
        let root_session_id = SessionId::new();
        let openai = OpenAi::builder("test-key")
            .service(|| CompletedSpineService)
            .build()?;
        let tools = Tools::builder().without_defaults().build()?;
        let (intent_sink, intents) = SpineIntentChannel::new();
        let tool_sink: Arc<dyn SpineIntentSink> = intent_sink.clone();
        let (root, _root_events) = Nanocodex::builder(openai)
            .thinking(Thinking::Low)
            .workspace(directory)
            .session_id(root_session_id)
            .rollout(RolloutConfig::new(directory))
            .tools_factory(move |_agent| with_spine_tools(tools.clone(), Arc::clone(&tool_sink)))
            .build()?;
        let root_session_id = root.session_id().to_string();
        let root_turn = root.prompt("establish the durable root boundary").await?;
        root_turn.result().await?;
        root.flush_rollout().await?;
        let (agent, events) = root.fork().await?;
        let active_session_id = agent.session_id().to_string();
        root.shutdown().await?;
        let runtime = Arc::new(SpineRuntime::create(
            SpineRuntimeLimits::default(),
            directory.join("spine").as_path(),
            &root_session_id,
            &root_session_id,
            "2026-08-10T00:00:00Z",
        )?);
        let transition = runtime.prepare(SpineIntentRequest::new(
            root_session_id.clone(),
            "manual-delivery",
            nanocodex_spine_runtime::SpineTerminalControl::Open {
                summary: "continue after recovery".to_owned(),
            },
        ))?;
        let delivery = runtime.commit(
            &transition,
            active_session_id.clone(),
            None,
            "delivery-manual",
        )?;
        runtime.claim_delivery(&delivery)?;
        let cli = Cli::try_parse_from([
            "nanocodex-spine",
            "--api-key",
            "test-key",
            "--browser=none",
            "--subagents",
            "false",
        ])?;
        let session_recipe = SpineSessionRecipe::new(
            cli.agent,
            cli.vm,
            Arc::clone(&intent_sink),
            directory.to_path_buf(),
        );
        let configured = ConfiguredAgent {
            handle: agent,
            events,
            realtime: None,
            child_agents: None,
            mpp_adapter: None,
            mcp: None,
            browser: None,
            vm: None,
        };
        let initial = SpineWorkerInitial {
            initial_delivery: None,
            manual_delivery: Some(delivery.clone()),
            initial_status: None,
            root_session_id: root_session_id.clone(),
            active_session_id,
            capabilities: capabilities(),
        };
        let worker = match delivery_fault {
            Some(fault) => SpineWorker::start_with_delivery_fault(
                configured,
                Arc::clone(&runtime),
                intents,
                session_recipe,
                initial,
                fault,
            ),
            None => SpineWorker::start(
                configured,
                Arc::clone(&runtime),
                intents,
                session_recipe,
                initial,
            ),
        };
        Ok((worker, runtime, delivery))
    }

    fn spine_cli_with_websocket(endpoint: &str) -> eyre::Result<Cli> {
        Cli::try_parse_from([
            "nanocodex-spine",
            "--api-key",
            "test-key",
            "--browser=none",
            "--subagents",
            "false",
            "--websocket-url",
            endpoint,
        ])
        .map_err(Into::into)
    }

    async fn completed_responses_server(
        connections: usize,
    ) -> eyre::Result<(String, tokio::task::JoinHandle<eyre::Result<()>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("ws://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            for connection in 0..connections {
                let (stream, _) = listener.accept().await?;
                let mut socket = accept_async(stream).await?;
                let warmup = next_ws_json(&mut socket).await?;
                assert_eq!(warmup["generate"], false, "connection {connection}");
                send_ws_json(
                    &mut socket,
                    serde_json::json!({
                        "type": "response.completed",
                        "response": { "id": format!("resp-warmup-{connection}"), "usage": null }
                    }),
                )
                .await?;
                let generation = next_ws_json(&mut socket).await?;
                assert_ne!(generation["generate"], false, "connection {connection}");
                send_ws_json(
                    &mut socket,
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": format!("resp-generation-{connection}"),
                            "status": "completed",
                            "output": [{
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": "completed" }]
                            }],
                            "usage": null
                        }
                    }),
                )
                .await?;
            }
            Ok(())
        });
        Ok((endpoint, server))
    }

    async fn wait_for_main_turn(
        worker: &mut crate::app_core::WorkerHandle<WorkerCommand, WorkerEvent>,
    ) -> eyre::Result<()> {
        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    worker.events_mut().recv().await,
                    Some(WorkerEvent::TurnFinished {
                        target: PaneId::Main,
                        main_branch_id: Some(0),
                        error: None,
                    })
                ) {
                    return Ok::<(), eyre::Report>(());
                }
            }
        })
        .await??;
        Ok(())
    }

    async fn next_ws_json<S>(socket: &mut WebSocketStream<S>) -> eyre::Result<serde_json::Value>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        loop {
            let message = socket
                .next()
                .await
                .ok_or_else(|| eyre!("client closed before sending a request"))??;
            if let Message::Text(text) = message {
                return Ok(serde_json::from_str(text.as_str())?);
            }
        }
    }

    async fn send_ws_json<S>(
        socket: &mut WebSocketStream<S>,
        value: serde_json::Value,
    ) -> eyre::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        socket.send(Message::Text(value.to_string().into())).await?;
        Ok(())
    }

    #[tokio::test]
    async fn cold_close_releases_the_child_bundle_before_restoring_the_parent() -> eyre::Result<()>
    {
        let directory = tempdir()?;
        let generation_calls = Arc::new(AtomicU32::new(0));
        let parent_resumed = Arc::new(Notify::new());
        let openai = OpenAi::builder("test-key")
            .service({
                let generation_calls = Arc::clone(&generation_calls);
                let parent_resumed = Arc::clone(&parent_resumed);
                move || ColdCloseService {
                    generation_calls: Arc::clone(&generation_calls),
                    parent_resumed: Arc::clone(&parent_resumed),
                }
            })
            .build()?;
        let tools = Tools::builder().without_defaults().build()?;
        let (intent_sink, intents) = SpineIntentChannel::new();
        let tool_sink: Arc<dyn SpineIntentSink> = intent_sink.clone();
        let (root, _root_events) = Nanocodex::builder(openai.clone())
            .thinking(Thinking::Low)
            .workspace(directory.path())
            .rollout(RolloutConfig::new(directory.path()))
            .tools_factory({
                let tools = tools.clone();
                let tool_sink = Arc::clone(&tool_sink);
                move |_agent| with_spine_tools(tools.clone(), Arc::clone(&tool_sink))
            })
            .build()?;
        let root_session_id = root.session_id().to_string();
        let root_turn = root.prompt("make the parent durable").await?;
        root_turn.result().await?;
        root.flush_rollout().await?;
        let (child, child_events) = root.fork().await?;
        let child_session_id = child.session_id().to_string();
        root.shutdown().await?;

        let runtime = Arc::new(SpineRuntime::create(
            SpineRuntimeLimits::default(),
            directory.path().join("spine").as_path(),
            &root_session_id,
            &root_session_id,
            "2026-08-09T00:00:00Z",
        )?);
        let open = runtime.prepare(SpineIntentRequest::new(
            root_session_id.clone(),
            "call-open",
            nanocodex_spine_runtime::SpineTerminalControl::Open {
                summary: "inspect the parser".to_owned(),
            },
        ))?;
        let open_delivery =
            runtime.commit(&open, child_session_id.clone(), None, "delivery-open")?;
        runtime.claim_delivery(&open_delivery)?;
        runtime.accept_delivery(&open_delivery)?;

        let cli = Cli::try_parse_from(["nanocodex-spine", "--api-key", "test-key"])?;
        let resumed_openai = openai.clone();
        let resumed_tools = tools.clone();
        let resumed_sink = Arc::clone(&tool_sink);
        let resumed_builder: TestResumedAgentBuilder = Arc::new(move |durable| {
            let openai = resumed_openai.clone();
            let tools = resumed_tools.clone();
            let tool_sink = Arc::clone(&resumed_sink);
            Box::pin(async move {
                let workspace = PathBuf::from(durable.workspace());
                let (thread_id, snapshot, rollout) = durable.into_parts();
                let session_id = thread_id
                    .parse::<SessionId>()
                    .map_err(|error| eyre!(error))?;
                let (handle, events) = Nanocodex::builder(openai)
                    .thinking(Thinking::Low)
                    .workspace(workspace)
                    .session_id(session_id)
                    .resume(snapshot)
                    .rollout(rollout)
                    .tools_factory(move |_agent| {
                        with_spine_tools(tools.clone(), Arc::clone(&tool_sink))
                    })
                    .build()?;
                Ok(ConfiguredAgent {
                    handle,
                    events,
                    realtime: None,
                    child_agents: None,
                    mpp_adapter: None,
                    mcp: None,
                    browser: None,
                    vm: None,
                })
            })
        });
        let session_recipe = SpineSessionRecipe::with_resumed_builder(
            cli.agent,
            cli.vm,
            Arc::clone(&intent_sink),
            directory.path().to_path_buf(),
            resumed_builder,
        );
        let configured = ConfiguredAgent {
            handle: child,
            events: child_events,
            realtime: None,
            child_agents: None,
            mpp_adapter: None,
            mcp: None,
            browser: None,
            vm: None,
        };
        let mut worker = SpineWorker::start(
            configured,
            Arc::clone(&runtime),
            intents,
            session_recipe,
            SpineWorkerInitial {
                initial_delivery: None,
                manual_delivery: None,
                initial_status: None,
                root_session_id: root_session_id.clone(),
                active_session_id: child_session_id,
                capabilities: capabilities(),
            },
        );

        worker.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 1,
            prompt: "finish the child".into(),
        })?;
        let child_result = match timeout(Duration::from_secs(5), async {
            loop {
                match worker.events_mut().recv().await {
                    Some(WorkerEvent::TurnFinished {
                        target: PaneId::Main,
                        main_branch_id: Some(0),
                        error,
                    }) => return error,
                    Some(_) => {}
                    None => panic!("Spine worker stopped before the child turn finished"),
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let error = worker
                    .shutdown()
                    .await
                    .expect_err("the cold-close worker should report its failed transition");
                return Err(eyre!("cold-close worker stopped: {error}"));
            }
        };
        if let Some(error) = child_result {
            return Err(eyre!("cold child close failed: {error}"));
        }
        timeout(Duration::from_secs(5), parent_resumed.notified()).await?;

        assert_eq!(runtime.active_session_id()?, root_session_id);
        assert_eq!(runtime.projection()?.cursor.to_string(), "1");
        assert_eq!(generation_calls.load(Ordering::Relaxed), 3);
        worker.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn cold_next_rebuilds_the_frozen_parent_before_committing_a_sibling() -> eyre::Result<()>
    {
        let directory = tempdir()?;
        let generation_calls = Arc::new(AtomicU32::new(0));
        let parent_resumed = Arc::new(Notify::new());
        let openai = OpenAi::builder("test-key")
            .service({
                let generation_calls = Arc::clone(&generation_calls);
                let parent_resumed = Arc::clone(&parent_resumed);
                move || ColdNextService {
                    generation_calls: Arc::clone(&generation_calls),
                    parent_resumed: Arc::clone(&parent_resumed),
                }
            })
            .build()?;
        let tools = Tools::builder().without_defaults().build()?;
        let (intent_sink, intents) = SpineIntentChannel::new();
        let tool_sink: Arc<dyn SpineIntentSink> = intent_sink.clone();
        let (root, _root_events) = Nanocodex::builder(openai.clone())
            .thinking(Thinking::Low)
            .workspace(directory.path())
            .rollout(RolloutConfig::new(directory.path()))
            .tools_factory({
                let tools = tools.clone();
                let tool_sink = Arc::clone(&tool_sink);
                move |_agent| with_spine_tools(tools.clone(), Arc::clone(&tool_sink))
            })
            .build()?;
        let root_session_id = root.session_id().to_string();
        let root_turn = root.prompt("make the parent durable").await?;
        root_turn.result().await?;
        root.flush_rollout().await?;
        let (child, child_events) = root.fork().await?;
        let child_session_id = child.session_id().to_string();
        root.shutdown().await?;

        let runtime = Arc::new(SpineRuntime::create(
            SpineRuntimeLimits::default(),
            directory.path().join("spine").as_path(),
            &root_session_id,
            &root_session_id,
            "2026-08-09T00:00:00Z",
        )?);
        let open = runtime.prepare(SpineIntentRequest::new(
            root_session_id.clone(),
            "call-open",
            nanocodex_spine_runtime::SpineTerminalControl::Open {
                summary: "inspect the parser".to_owned(),
            },
        ))?;
        let open_delivery =
            runtime.commit(&open, child_session_id.clone(), None, "delivery-open")?;
        runtime.claim_delivery(&open_delivery)?;
        runtime.accept_delivery(&open_delivery)?;

        let cli = Cli::try_parse_from(["nanocodex-spine", "--api-key", "test-key"])?;
        let resumed_openai = openai.clone();
        let resumed_tools = tools.clone();
        let resumed_sink = Arc::clone(&tool_sink);
        let resumed_builder: TestResumedAgentBuilder = Arc::new(move |durable| {
            let openai = resumed_openai.clone();
            let tools = resumed_tools.clone();
            let tool_sink = Arc::clone(&resumed_sink);
            Box::pin(async move {
                let workspace = PathBuf::from(durable.workspace());
                let (thread_id, snapshot, rollout) = durable.into_parts();
                let session_id = thread_id
                    .parse::<SessionId>()
                    .map_err(|error| eyre!(error))?;
                let (handle, events) = Nanocodex::builder(openai)
                    .thinking(Thinking::Low)
                    .workspace(workspace)
                    .session_id(session_id)
                    .resume(snapshot)
                    .rollout(rollout)
                    .tools_factory(move |_agent| {
                        with_spine_tools(tools.clone(), Arc::clone(&tool_sink))
                    })
                    .build()?;
                Ok(ConfiguredAgent {
                    handle,
                    events,
                    realtime: None,
                    child_agents: None,
                    mpp_adapter: None,
                    mcp: None,
                    browser: None,
                    vm: None,
                })
            })
        });
        let session_recipe = SpineSessionRecipe::with_resumed_builder(
            cli.agent,
            cli.vm,
            Arc::clone(&intent_sink),
            directory.path().to_path_buf(),
            resumed_builder,
        );
        let configured = ConfiguredAgent {
            handle: child,
            events: child_events,
            realtime: None,
            child_agents: None,
            mpp_adapter: None,
            mcp: None,
            browser: None,
            vm: None,
        };
        let mut worker = SpineWorker::start(
            configured,
            Arc::clone(&runtime),
            intents,
            session_recipe,
            SpineWorkerInitial {
                initial_delivery: None,
                manual_delivery: None,
                initial_status: None,
                root_session_id: root_session_id.clone(),
                active_session_id: child_session_id,
                capabilities: capabilities(),
            },
        );

        worker.commands().send(WorkerCommand::Prompt {
            target: PaneId::Main,
            prompt_id: 1,
            prompt: "replace the child scope".into(),
        })?;
        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    worker.events_mut().recv().await,
                    Some(WorkerEvent::TurnFinished {
                        target: PaneId::Main,
                        main_branch_id: Some(0),
                        error: None,
                    })
                ) {
                    return;
                }
            }
        })
        .await?;
        timeout(Duration::from_secs(5), parent_resumed.notified()).await?;

        let projection = runtime.projection()?;
        assert_eq!(runtime.active_session_id()?, root_session_id);
        assert_eq!(projection.cursor.to_string(), "1");
        assert_eq!(projection.nodes.len(), 3);
        assert_eq!(generation_calls.load(Ordering::Relaxed), 4);
        worker.shutdown().await?;
        Ok(())
    }

    #[derive(Clone)]
    struct TerminalSpineService {
        calls: Arc<AtomicU32>,
        parent_finished: Arc<Notify>,
    }

    #[derive(Clone, Default)]
    struct DeliveryFaultService {
        generation_calls: Arc<AtomicU32>,
    }

    #[derive(Clone, Copy)]
    struct CompletedSpineService;

    #[derive(Clone)]
    struct ToolProfileService {
        profiles: Arc<Mutex<Vec<BTreeSet<String>>>>,
    }

    #[derive(Clone)]
    struct ColdCloseService {
        generation_calls: Arc<AtomicU32>,
        parent_resumed: Arc<Notify>,
    }

    #[derive(Clone)]
    struct ColdNextService {
        generation_calls: Arc<AtomicU32>,
        parent_resumed: Arc<Notify>,
    }

    impl Service<ResponsesAttempt> for ColdCloseService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = Ready<Result<ResponsesServiceResponse, ResponseError>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
            let output = match request.kind() {
                ResponsesAttemptKind::Warmup => ResponsesOutput::Warmup(WarmupResponse {
                    id: "resp-warmup".to_owned(),
                    usage: None,
                }),
                ResponsesAttemptKind::Generation => {
                    match self.generation_calls.fetch_add(1, Ordering::Relaxed) {
                        0 => final_generation("resp-root-ready", "the parent is durable"),
                        1 => code_generation(
                            "resp-child-close",
                            "call-child-close",
                            "await tools.spine__close({memory: 'the parser needs one-token lookahead'});",
                        ),
                        2 => {
                            assert_request_contains(
                                &request,
                                "the parser needs one-token lookahead",
                            );
                            self.parent_resumed.notify_one();
                            final_generation("resp-parent-resumed", "parent resumed after restart")
                        }
                        call => panic!("unexpected scripted generation {call}"),
                    }
                }
                _ => panic!("unexpected scripted Responses attempt"),
            };
            ready(Ok(ResponsesServiceResponse::new(output)))
        }
    }

    impl Service<ResponsesAttempt> for ColdNextService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = Ready<Result<ResponsesServiceResponse, ResponseError>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
            let output = match request.kind() {
                ResponsesAttemptKind::Warmup => ResponsesOutput::Warmup(WarmupResponse {
                    id: "resp-warmup".to_owned(),
                    usage: None,
                }),
                ResponsesAttemptKind::Generation => {
                    match self.generation_calls.fetch_add(1, Ordering::Relaxed) {
                        0 => final_generation("resp-root-ready", "the parent is durable"),
                        1 => code_generation(
                            "resp-child-next",
                            "call-child-next",
                            "await tools.spine__next({summary: 'write a parser fix', memory: 'the parser needs one-token lookahead'});",
                        ),
                        2 => {
                            assert_request_contains(&request, "Scope:\\nwrite a parser fix");
                            assert_request_contains(
                                &request,
                                "the parser needs one-token lookahead",
                            );
                            code_generation(
                                "resp-sibling-close",
                                "call-sibling-close",
                                "await tools.spine__close({memory: 'the parser fix is complete'});",
                            )
                        }
                        3 => {
                            assert_request_contains(&request, "the parser fix is complete");
                            self.parent_resumed.notify_one();
                            final_generation("resp-parent-resumed", "parent resumed after restart")
                        }
                        call => panic!("unexpected scripted generation {call}"),
                    }
                }
                _ => panic!("unexpected scripted Responses attempt"),
            };
            ready(Ok(ResponsesServiceResponse::new(output)))
        }
    }

    impl Service<ResponsesAttempt> for TerminalSpineService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = Ready<Result<ResponsesServiceResponse, ResponseError>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            let output = match (call, request.kind()) {
                (0, ResponsesAttemptKind::Warmup) => ResponsesOutput::Warmup(WarmupResponse {
                    id: "resp-warmup".to_owned(),
                    usage: None,
                }),
                (1, ResponsesAttemptKind::Generation) => code_generation(
                    "resp-root-open",
                    "call-root-exec",
                    "await tools.spine__open({summary: 'inspect the parser'});",
                ),
                (2, ResponsesAttemptKind::Generation) => {
                    assert_request_contains(&request, "Scope:\\ninspect the parser");
                    code_generation(
                        "resp-child-next",
                        "call-child-exec",
                        "await tools.spine__next({summary: 'write a parser fix', memory: 'parser accepts one token too eagerly'});",
                    )
                }
                (3, ResponsesAttemptKind::Generation) => {
                    assert_request_contains(&request, "Scope:\\nwrite a parser fix");
                    assert_request_contains(&request, "parser accepts one token too eagerly");
                    code_generation(
                        "resp-sibling-close",
                        "call-sibling-exec",
                        "await tools.spine__close({memory: 'the parser fix needs one-token lookahead'});",
                    )
                }
                (4, ResponsesAttemptKind::Generation) => {
                    assert_request_contains(&request, "the parser fix needs one-token lookahead");
                    self.parent_finished.notify_one();
                    final_generation("resp-parent-finished", "parent resumed")
                }
                _ => panic!("unexpected scripted Responses attempt {call}"),
            };
            ready(Ok(ResponsesServiceResponse::new(output)))
        }
    }

    impl Service<ResponsesAttempt> for DeliveryFaultService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = Ready<Result<ResponsesServiceResponse, ResponseError>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
            let output = match request.kind() {
                ResponsesAttemptKind::Warmup => ResponsesOutput::Warmup(WarmupResponse {
                    id: "resp-warmup".to_owned(),
                    usage: None,
                }),
                ResponsesAttemptKind::Generation => {
                    if self.generation_calls.fetch_add(1, Ordering::Relaxed) == 0 {
                        code_generation(
                            "resp-root-open",
                            "call-root-open",
                            "await tools.spine__open({summary: 'inspect the parser'});",
                        )
                    } else {
                        final_generation("resp-unexpected-continuation", "continuation started")
                    }
                }
                _ => panic!("unexpected scripted Responses attempt"),
            };
            ready(Ok(ResponsesServiceResponse::new(output)))
        }
    }

    impl Service<ResponsesAttempt> for CompletedSpineService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = Ready<Result<ResponsesServiceResponse, ResponseError>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
            let output = match request.kind() {
                ResponsesAttemptKind::Warmup => ResponsesOutput::Warmup(WarmupResponse {
                    id: "resp-warmup".to_owned(),
                    usage: None,
                }),
                ResponsesAttemptKind::Generation => {
                    final_generation("resp-completed", "completed manual delivery")
                }
                _ => panic!("unexpected scripted Responses attempt"),
            };
            ready(Ok(ResponsesServiceResponse::new(output)))
        }
    }

    impl Service<ResponsesAttempt> for ToolProfileService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = Ready<Result<ResponsesServiceResponse, ResponseError>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
            let output = match request.kind() {
                ResponsesAttemptKind::Warmup => ResponsesOutput::Warmup(WarmupResponse {
                    id: "resp-warmup".to_owned(),
                    usage: None,
                }),
                ResponsesAttemptKind::Generation => {
                    self.profiles
                        .lock()
                        .expect("tool profile lock")
                        .push(request_tool_names(&request));
                    final_generation("resp-tool-profile", "completed")
                }
                _ => panic!("unexpected tool profile Responses attempt"),
            };
            ready(Ok(ResponsesServiceResponse::new(output)))
        }
    }

    fn request_tool_names(request: &ResponsesAttempt) -> BTreeSet<String> {
        let profile = nanocodex_oai_api::__private::test_support::request_profile(request);
        profile
            .prefix()
            .iter()
            .chain(request.input_items())
            .filter_map(|item| match item {
                ResponseItem::AdditionalTools { tools, .. } => Some(tools),
                _ => None,
            })
            .flatten()
            .map(|tool| tool.name().to_owned())
            .chain(profile.code_mode_tool_names().map(str::to_owned))
            .collect()
    }

    fn assert_spine_coordinator_tools(profile: &BTreeSet<String>) {
        for name in ["spine__open", "spine__close", "spine__next"] {
            assert!(profile.contains(name), "coordinator is missing {name}");
        }
        assert_subagent_tools(profile);
    }

    fn assert_ordinary_child_tools(profile: &BTreeSet<String>) {
        assert!(
            profile.iter().all(|name| !name.starts_with("spine__")),
            "ordinary child unexpectedly received Spine controls: {profile:?}"
        );
        assert_subagent_tools(profile);
    }

    fn assert_subagent_tools(profile: &BTreeSet<String>) {
        for name in ["spawn_agent", "fork_agent", "prompt_agent"] {
            assert!(profile.contains(name), "child is missing {name}");
        }
    }

    fn assert_request_contains(request: &ResponsesAttempt, expected: &str) {
        let input = serde_json::to_string(&request.input_items().collect::<Vec<_>>())
            .expect("serialize request input");
        assert!(
            input.contains(expected),
            "request did not contain {expected:?}: {input}"
        );
    }

    fn code_generation(response_id: &str, call_id: &str, source: &str) -> ResponsesOutput {
        let output_item = serde_json::from_value(serde_json::json!({
            "type": "custom_tool_call",
            "call_id": call_id,
            "name": "exec",
            "input": source,
        }))
        .expect("custom tool call response item");
        ResponsesOutput::Generation(GenerationOutput {
            id: response_id.to_owned(),
            status: "completed".to_owned(),
            end_turn: Some(false),
            final_message: None,
            output_items: vec![output_item],
            code_calls: vec![CodeCall {
                call_id: call_id.to_owned(),
                name: "exec".to_owned(),
                namespace: None,
                input: source.to_owned(),
                kind: CodeCallKind::Custom,
            }],
            usage: Some(Usage::default()),
            time_to_first_event_ns: 0,
            time_to_first_output_ns: None,
            pipeline_stats: ResponsePipelineStats::default(),
        })
    }

    fn final_generation(response_id: &str, message: &str) -> ResponsesOutput {
        ResponsesOutput::Generation(GenerationOutput {
            id: response_id.to_owned(),
            status: "completed".to_owned(),
            end_turn: Some(true),
            final_message: Some(message.to_owned()),
            output_items: vec![ResponseItem::message(
                MessageRole::Assistant,
                [ContentItem::output_text(message)],
            )],
            code_calls: Vec::new(),
            usage: Some(Usage::default()),
            time_to_first_event_ns: 0,
            time_to_first_output_ns: None,
            pipeline_stats: ResponsePipelineStats::default(),
        })
    }
}
