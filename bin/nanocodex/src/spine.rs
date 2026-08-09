use std::{path::PathBuf, process::ExitCode, sync::Arc};

use clap::{Parser, Subcommand, builder::NonEmptyStringValueParser};
use eyre::{Result, eyre};
use nanocodex::agent::rollout::RolloutTranscriptItem;
use nanocodex_spine_runtime::{SpineDelivery, SpineRuntime, SpineRuntimeLimits};
use tokio::sync::mpsc;

use crate::{
    app_core::{WorkerFactory, WorkerHandle},
    config::{AgentArgs, ConfiguredAgent, default_codex_home},
    observability::ObservabilityArgs,
    spine_worker::{
        SpineIntentChannel, SpineSessionRecipe, SpineWorker, capabilities, durable_prompt_cache_key,
    },
    tui::{self, InitialPrompt, WorkerCommand, WorkerEvent},
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

async fn run(cli: Cli) -> Result<()> {
    if cli.agent.subagents_enabled() {
        return Err(eyre!(
            "nanocodex-spine does not support --subagents; use the synchronous Spine continuation tools instead"
        ));
    }
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
        Some(Command::Resume { root_thread_id }) => (
            SpineWorkerFactory::resume(cli.agent, cli.vm, &root_thread_id).await?,
            None,
        ),
        None => (
            SpineWorkerFactory::build(cli.agent, cli.vm).await?,
            cli.prompt.map(InitialPrompt::plain),
        ),
    };
    let initial = factory.initial.clone();
    tui::run_with_worker(
        factory,
        initial.cwd,
        initial.model,
        initial.thinking,
        initial.fast_mode,
        initial.transcript,
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
    initial: SpineInitial,
}

#[derive(Clone)]
struct SpineInitial {
    cwd: PathBuf,
    model: nanocodex::Model,
    thinking: nanocodex::Thinking,
    fast_mode: bool,
    transcript: Vec<RolloutTranscriptItem>,
}

impl SpineWorkerFactory {
    async fn build(config: AgentArgs, vm: VmArgs) -> Result<Self> {
        let cwd = config.cwd().to_path_buf();
        let initial = SpineInitial {
            cwd,
            model: config.model(),
            thinking: config.thinking(),
            fast_mode: config.fast_mode(),
            transcript: Vec::new(),
        };
        let codex_home = default_codex_home()?;
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
            initial,
        })
    }

    async fn resume(config: AgentArgs, vm: VmArgs, root_thread_id: &str) -> Result<Self> {
        uuid::Uuid::parse_str(root_thread_id)
            .map_err(|error| eyre!("invalid Spine root thread ID `{root_thread_id}`: {error}"))?;
        let codex_home = default_codex_home()?;
        let journal_directory = codex_home.join("spine");
        let runtime = Arc::new(
            SpineRuntime::open(
                SpineRuntimeLimits::default(),
                &journal_directory,
                root_thread_id,
            )
            .map_err(|error| eyre!(error))?,
        );
        if let Some(pending) = runtime.pending_transition().map_err(|error| eyre!(error))? {
            runtime
                .abort_prepared(
                    &pending,
                    "the previous process stopped before this Spine transition committed",
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
        let durable = session_recipe.load(&active_session_id)?;
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
        Ok(Self {
            configured,
            runtime,
            intents,
            session_recipe,
            root_session_id,
            active_session_id,
            initial_delivery,
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
            self.initial_delivery,
            self.root_session_id,
            self.active_session_id,
            capabilities(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Ready, ready},
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };

    use clap::Parser;
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
        SpineIntentSink, SpineRuntime, SpineRuntimeLimits, with_spine_tools,
    };
    use tempfile::tempdir;
    use tokio::{sync::Notify, time::timeout};
    use tower::Service;

    use super::{Cli, run};
    use crate::{
        config::ConfiguredAgent,
        spine_worker::{SpineIntentChannel, SpineSessionRecipe, SpineWorker, capabilities},
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

    #[tokio::test]
    async fn spine_cli_rejects_legacy_subagents_before_startup() {
        let cli = Cli::try_parse_from(["nanocodex-spine", "--subagents", "true"]).unwrap();

        let error = run(cli).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "nanocodex-spine does not support --subagents; use the synchronous Spine continuation tools instead"
        );
    }

    #[tokio::test]
    async fn spine_cli_requires_standard_rollout_recording() {
        let cli = Cli::try_parse_from(["nanocodex-spine", "--rollouts", "false"]).unwrap();

        let error = run(cli).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "nanocodex-spine requires standard rollout recording; remove --rollouts false"
        );
    }

    #[tokio::test]
    async fn terminal_open_and_close_switch_the_durable_current_node() -> eyre::Result<()> {
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
        let cli = Cli::try_parse_from(["nanocodex-spine", "--api-key", "test-key"])?;
        let session_recipe = SpineSessionRecipe::new(
            cli.agent,
            cli.vm,
            Arc::clone(&intent_sink),
            directory.path().to_path_buf(),
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
        let mut worker = SpineWorker::start(
            configured,
            Arc::clone(&runtime),
            intents,
            session_recipe,
            None,
            root_session_id.clone(),
            root_session_id.clone(),
            capabilities(),
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
        assert_eq!(projection.nodes.len(), 2);
        assert_eq!(
            projection.nodes[1].summary.as_deref(),
            Some("inspect the parser")
        );
        assert_eq!(calls.load(Ordering::Relaxed), 4);

        worker.shutdown().await?;
        Ok(())
    }

    #[derive(Clone)]
    struct TerminalSpineService {
        calls: Arc<AtomicU32>,
        parent_finished: Arc<Notify>,
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
                        "resp-child-close",
                        "call-child-exec",
                        "await tools.spine__close({memory: 'parser accepts one token too eagerly'});",
                    )
                }
                (3, ResponsesAttemptKind::Generation) => {
                    assert_request_contains(&request, "parser accepts one token too eagerly");
                    self.parent_finished.notify_one();
                    final_generation("resp-parent-finished", "parent resumed")
                }
                _ => panic!("unexpected scripted Responses attempt {call}"),
            };
            ready(Ok(ResponsesServiceResponse::new(output)))
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
