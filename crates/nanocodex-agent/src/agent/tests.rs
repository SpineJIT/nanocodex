use std::{
    future::{Future, ready},
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use nanocodex_oai_api::{
    events::AgentEventKind,
    responses::{ContentItem, MessageRole, ResponseItem, ResponseItemId, Usage, WarmupResponse},
    tower::{
        CompactionOutput, GenerationOutput, ResponsePipelineStats, ResponsesAttemptKind,
        ResponsesOutput,
    },
};
use tempfile::tempdir;
use tokio::sync::{Notify, mpsc, oneshot};
use tower::Service;

use super::*;
use crate::rollout::RolloutConfig;

#[derive(Clone)]
struct DelayedCompletedService {
    generation_started: mpsc::UnboundedSender<()>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    generation_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone)]
struct DelayedCompactionService {
    compaction_started: mpsc::UnboundedSender<()>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    compaction_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone)]
struct CheckpointLifecycleService {
    generation_inputs: Arc<Mutex<Vec<Vec<ResponseItem>>>>,
}

impl Service<ResponsesAttempt> for CheckpointLifecycleService {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = Pin<
        Box<
            dyn Future<Output = std::result::Result<ResponsesServiceResponse, ResponseError>>
                + Send,
        >,
    >;

    fn poll_ready(
        &mut self,
        _context: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
        let output = match request.kind() {
            ResponsesAttemptKind::Warmup => ResponsesOutput::Warmup(WarmupResponse {
                id: "resp-warmup".to_owned(),
                usage: None,
            }),
            ResponsesAttemptKind::Compaction => ResponsesOutput::Compaction(CompactionOutput {
                id: "resp-compaction".to_owned(),
                status: "completed".to_owned(),
                item: ResponseItem::Compaction {
                    id: Some(ResponseItemId::from("cmp-durable-fork")),
                    encrypted_content: "opaque-summary".into(),
                    created_by: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                usage: None,
                time_to_first_event_ns: 0,
                time_to_first_output_ns: None,
                pipeline_stats: ResponsePipelineStats::default(),
            }),
            ResponsesAttemptKind::Generation => {
                self.generation_inputs
                    .lock()
                    .expect("generation input lock")
                    .push(request.input_items().cloned().collect());
                ResponsesOutput::Generation(GenerationOutput {
                    id: "resp-generation".to_owned(),
                    status: "completed".to_owned(),
                    end_turn: Some(true),
                    final_message: Some("done".to_owned()),
                    output_items: vec![ResponseItem::message(
                        MessageRole::Assistant,
                        [ContentItem::output_text("done")],
                    )],
                    code_calls: Vec::new(),
                    usage: None,
                    time_to_first_event_ns: 0,
                    time_to_first_output_ns: None,
                    pipeline_stats: ResponsePipelineStats::default(),
                })
            }
            _ => panic!("durable fork test received an unsupported attempt"),
        };
        Box::pin(ready(Ok(ResponsesServiceResponse::new(output))))
    }
}

impl Service<ResponsesAttempt> for DelayedCompactionService {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = Pin<
        Box<
            dyn Future<Output = std::result::Result<ResponsesServiceResponse, ResponseError>>
                + Send,
        >,
    >;

    fn poll_ready(
        &mut self,
        _context: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
        match request.kind() {
            ResponsesAttemptKind::Compaction => {
                use std::sync::atomic::Ordering;

                self.compaction_calls.fetch_add(1, Ordering::Relaxed);
                self.compaction_started
                    .send(())
                    .expect("test observes the compaction attempt");
                let release = self
                    .release
                    .lock()
                    .expect("release lock")
                    .take()
                    .expect("one compaction waits for release");
                Box::pin(async move {
                    release.await.expect("test releases compaction");
                    Ok(ResponsesServiceResponse::new(ResponsesOutput::Compaction(
                        CompactionOutput {
                            id: "resp-compaction".to_owned(),
                            status: "completed".to_owned(),
                            item: ResponseItem::Compaction {
                                id: Some(ResponseItemId::from("cmp-persist-failure")),
                                encrypted_content: "opaque-summary".into(),
                                created_by: None,
                                internal_chat_message_metadata_passthrough: None,
                            },
                            usage: None,
                            time_to_first_event_ns: 0,
                            time_to_first_output_ns: None,
                            pipeline_stats: ResponsePipelineStats::default(),
                        },
                    )))
                })
            }
            ResponsesAttemptKind::Warmup => Box::pin(ready(Ok(ResponsesServiceResponse::new(
                ResponsesOutput::Warmup(WarmupResponse {
                    id: "resp-warmup".to_owned(),
                    usage: None,
                }),
            )))),
            _ => panic!("compaction persistence failure test received an unsupported attempt"),
        }
    }
}

impl Service<ResponsesAttempt> for DelayedCompletedService {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = Pin<
        Box<
            dyn Future<Output = std::result::Result<ResponsesServiceResponse, ResponseError>>
                + Send,
        >,
    >;

    fn poll_ready(
        &mut self,
        _context: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
        match request.kind() {
            ResponsesAttemptKind::Warmup => Box::pin(ready(Ok(ResponsesServiceResponse::new(
                ResponsesOutput::Warmup(WarmupResponse {
                    id: "resp-warmup".to_owned(),
                    usage: None,
                }),
            )))),
            ResponsesAttemptKind::Generation => {
                use std::sync::atomic::Ordering;

                self.generation_calls.fetch_add(1, Ordering::Relaxed);
                self.generation_started
                    .send(())
                    .expect("test observes the generation attempt");
                let release = self
                    .release
                    .lock()
                    .expect("release lock")
                    .take()
                    .expect("one generation waits for release");
                Box::pin(async move {
                    release.await.expect("test releases generation");
                    Ok(ResponsesServiceResponse::new(ResponsesOutput::Generation(
                        GenerationOutput {
                            id: "resp-generation".to_owned(),
                            status: "completed".to_owned(),
                            end_turn: Some(true),
                            final_message: Some("done".to_owned()),
                            output_items: vec![ResponseItem::message(
                                MessageRole::Assistant,
                                [ContentItem::output_text("done")],
                            )],
                            code_calls: Vec::new(),
                            usage: Some(Usage {
                                input_tokens: 1,
                                output_tokens: 1,
                                total_tokens: 2,
                                ..Usage::default()
                            }),
                            time_to_first_event_ns: 0,
                            time_to_first_output_ns: Some(0),
                            pipeline_stats: ResponsePipelineStats::default(),
                        },
                    )))
                })
            }
            ResponsesAttemptKind::Compaction => {
                panic!("persistence fail-stop test must not compact")
            }
            _ => panic!("persistence fail-stop test received an unsupported attempt"),
        }
    }
}

#[tokio::test]
async fn closed_command_channel_returns_recorded_persistence_failure() {
    let (commands, receiver) = mpsc::channel(1);
    let (blocked_result, _blocked_receiver) = oneshot::channel();
    commands
        .send(Command::SetFastMode {
            enabled: true,
            result: blocked_result,
        })
        .await
        .expect("fill the command channel");
    let failure = DriverFailure::default();
    let attempted_send = Arc::new(Notify::new());
    let request = tokio::spawn({
        let commands = commands.clone();
        let failure = failure.clone();
        let attempted_send = Arc::clone(&attempted_send);
        async move {
            request_command(&commands, &failure, move |result| {
                attempted_send.notify_one();
                Command::SetFastMode {
                    enabled: true,
                    result,
                }
            })
            .await
        }
    });
    attempted_send.notified().await;
    failure.record(crate::error::PersistRolloutFailure::new(
        PathBuf::from("rollout.jsonl"),
        std::io::Error::other("durability failed"),
    ));
    drop(receiver);

    assert!(matches!(
        request.await.expect("request task completes"),
        Err(NanocodexError::PersistRollout { .. })
    ));
}

#[tokio::test]
async fn rollout_persistence_failure_fail_stops_current_and_queued_turns() {
    use std::sync::atomic::Ordering;

    let home = tempdir().expect("temporary rollout home");
    let (generation_started, mut generation_started_rx) = mpsc::unbounded_channel();
    let (release, release_rx) = oneshot::channel();
    let generation_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let openai = OpenAi::builder("test")
        .service({
            let generation_started = generation_started.clone();
            let release = Arc::new(Mutex::new(Some(release_rx)));
            let generation_calls = Arc::clone(&generation_calls);
            move || DelayedCompletedService {
                generation_started: generation_started.clone(),
                release: Arc::clone(&release),
                generation_calls: Arc::clone(&generation_calls),
            }
        })
        .build()
        .expect("test OpenAI client");
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty tools");
    let (agent, mut events) = Nanocodex::builder(openai)
        .tools(tools)
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("agent with rollout");
    agent.durability.inject_write_failures(2).await;

    let first = agent
        .prompt("first durable turn")
        .await
        .expect("first turn");
    let control = first.control();
    generation_started_rx
        .recv()
        .await
        .expect("first generation starts");
    let queued = agent
        .prompt("must not run after persistence failure")
        .await
        .expect("queued turn");
    queued
        .cancel()
        .await
        .expect("cancel queued turn before persistence failure");
    release.send(()).expect("release first generation");

    assert!(matches!(
        first.result().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        queued.result().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.prompt("rejected after failed persistence").await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        control.steer("rejected after failed persistence").await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.fork().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.compact().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert_eq!(generation_calls.load(Ordering::Relaxed), 1);

    let mut terminals = Vec::new();
    while terminals.len() < 2 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("every accepted turn emits a terminal event")
            .expect("agent event stream remains open");
        if event.kind.is_terminal() {
            terminals.push(event.kind);
        }
    }
    assert_eq!(
        terminals,
        vec![AgentEventKind::RunFailed, AgentEventKind::RunFailed]
    );
    assert!(matches!(
        agent.flush_rollout().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.shutdown().await,
        Err(NanocodexError::Shutdown(error))
            if matches!(error.as_ref(), NanocodexError::PersistRollout { .. })
    ));
}

#[tokio::test]
async fn explicit_rollout_flush_failure_fail_stops_the_agent() {
    let home = tempdir().expect("temporary rollout home");
    let generation_inputs = Arc::new(Mutex::new(Vec::new()));
    let openai = OpenAi::builder("test")
        .service({
            let generation_inputs = Arc::clone(&generation_inputs);
            move || CheckpointLifecycleService {
                generation_inputs: Arc::clone(&generation_inputs),
            }
        })
        .build()
        .expect("test OpenAI client");
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty tools");
    let (agent, events) = Nanocodex::builder(openai)
        .tools(tools)
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("agent with rollout");
    agent.durability.inject_write_failures(1).await;

    assert!(matches!(
        agent.flush_rollout().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.prompt("must not run after failed flush").await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(
        generation_inputs
            .lock()
            .expect("generation input lock")
            .is_empty()
    );
    assert!(matches!(
        agent.flush_rollout().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.shutdown().await,
        Err(NanocodexError::Shutdown(error))
            if matches!(error.as_ref(), NanocodexError::PersistRollout { .. })
    ));
    drop(events);
}

#[tokio::test]
async fn rollout_flush_is_rejected_while_a_turn_is_active() {
    let home = tempdir().expect("temporary rollout home");
    let (generation_started, mut generation_started_rx) = mpsc::unbounded_channel();
    let (release, release_rx) = oneshot::channel();
    let generation_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let openai = OpenAi::builder("test")
        .service({
            let generation_started = generation_started.clone();
            let release = Arc::new(Mutex::new(Some(release_rx)));
            let generation_calls = Arc::clone(&generation_calls);
            move || DelayedCompletedService {
                generation_started: generation_started.clone(),
                release: Arc::clone(&release),
                generation_calls: Arc::clone(&generation_calls),
            }
        })
        .build()
        .expect("test OpenAI client");
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty tools");
    let (agent, events) = Nanocodex::builder(openai)
        .tools(tools)
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("agent with rollout");
    agent.durability.inject_write_failures(1).await;

    let turn = agent.prompt("durable turn").await.expect("accepted turn");
    generation_started_rx
        .recv()
        .await
        .expect("generation starts");
    assert!(matches!(
        agent.flush_rollout().await,
        Err(NanocodexError::InvalidRequest(_))
    ));
    release.send(()).expect("release generation");
    assert!(matches!(
        turn.result().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    drop(events);
}

#[tokio::test]
async fn compaction_persistence_failure_fail_stops_current_and_queued_turns() {
    use std::sync::atomic::Ordering;

    let home = tempdir().expect("temporary rollout home");
    let (compaction_started, mut compaction_started_rx) = mpsc::unbounded_channel();
    let (release, release_rx) = oneshot::channel();
    let compaction_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let openai = OpenAi::builder("test")
        .service({
            let compaction_started = compaction_started.clone();
            let release = Arc::new(Mutex::new(Some(release_rx)));
            let compaction_calls = Arc::clone(&compaction_calls);
            move || DelayedCompactionService {
                compaction_started: compaction_started.clone(),
                release: Arc::clone(&release),
                compaction_calls: Arc::clone(&compaction_calls),
            }
        })
        .build()
        .expect("test OpenAI client");
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty tools");
    let (agent, events) = Nanocodex::builder(openai)
        .tools(tools)
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("agent with rollout");
    agent.durability.inject_write_failures(2).await;

    let compact_agent = agent.clone();
    let compact = tokio::spawn(async move { compact_agent.compact().await });
    compaction_started_rx
        .recv()
        .await
        .expect("compaction starts");
    let queued = agent
        .prompt("must not run after compaction persistence failure")
        .await
        .expect("queued turn");
    release.send(()).expect("release compaction");

    assert!(matches!(
        compact.await.expect("compaction task joins"),
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        queued.result().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent
            .prompt("rejected after compaction persistence failure")
            .await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert_eq!(compaction_calls.load(Ordering::Relaxed), 1);
    assert!(matches!(
        agent.flush_rollout().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.shutdown().await,
        Err(NanocodexError::Shutdown(error))
            if matches!(error.as_ref(), NanocodexError::PersistRollout { .. })
    ));
    drop(events);
}

#[tokio::test]
async fn fork_seeds_a_resumable_child_rollout_with_the_inherited_identity() {
    let home = tempdir().expect("temporary rollout home");
    let (generation_started, mut generation_started_rx) = mpsc::unbounded_channel();
    let (release, release_rx) = oneshot::channel();
    let generation_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let openai = OpenAi::builder("test")
        .service({
            let generation_started = generation_started.clone();
            let release = Arc::new(Mutex::new(Some(release_rx)));
            let generation_calls = Arc::clone(&generation_calls);
            move || DelayedCompletedService {
                generation_started: generation_started.clone(),
                release: Arc::clone(&release),
                generation_calls: Arc::clone(&generation_calls),
            }
        })
        .build()
        .expect("test OpenAI client");
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty tools");
    let root_session_id = "019c0d31-c308-7d91-bff4-5dca82d15ac6";
    let (root, root_events) = Nanocodex::builder(openai)
        .tools(tools)
        .session_id(root_session_id.parse().expect("valid root session ID"))
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("root agent with rollout");
    let root_turn = root
        .prompt("durable parent boundary")
        .await
        .expect("root turn");
    generation_started_rx
        .recv()
        .await
        .expect("root generation starts");
    release.send(()).expect("release root generation");
    root_turn.result().await.expect("completed root turn");

    let (child, child_events) = root.fork().await.expect("durable fork");
    let child_rollout = child
        .rollout()
        .expect("fork records its own rollout")
        .path()
        .to_path_buf();
    let child_session_id = child.session_id().to_string();
    let lines = std::fs::read_to_string(&child_rollout)
        .expect("read child rollout")
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<serde_json::Result<Vec<_>>>()
        .expect("decode child rollout");
    let meta = lines
        .iter()
        .find(|line| line["type"] == "session_meta")
        .expect("child session metadata");
    assert_eq!(meta["payload"]["id"], child_session_id);
    assert_eq!(meta["payload"]["nanocodex_lineage_id"], root_session_id);
    assert_eq!(
        meta["payload"]["nanocodex_prompt_cache_key"],
        root_session_id
    );
    assert!(lines.iter().any(|line| line["type"] == "turn_context"));
    assert!(lines.iter().any(|line| line["type"] == "response_item"));
    assert!(lines.iter().any(|line| line["type"] == "world_state"));
    assert!(
        !lines.iter().any(|line| line["type"] == "event_msg"),
        "seeding must not manufacture a child task or user message"
    );

    let durable = RolloutConfig::new(home.path())
        .load_session(&child_session_id)
        .expect("child rollout is resumable before fork returns");
    let snapshot = serde_json::to_value(durable.snapshot()).expect("encode child snapshot");
    assert_eq!(snapshot["lineage_id"], root_session_id);
    assert_eq!(snapshot["prompt_cache_key"], root_session_id);
    assert!(
        snapshot["history"]
            .to_string()
            .contains("durable parent boundary")
    );

    child.shutdown().await.expect("shutdown child");
    root.shutdown().await.expect("shutdown root");
    drop((child_events, root_events));
}

#[tokio::test]
async fn failed_public_fork_seed_does_not_publish_a_child_rollout() {
    let home = tempdir().expect("temporary rollout home");
    let config = RolloutConfig::new(home.path()).fail_fork_seed_for_test();
    let generation_inputs = Arc::new(Mutex::new(Vec::new()));
    let openai = OpenAi::builder("test")
        .service({
            let generation_inputs = Arc::clone(&generation_inputs);
            move || CheckpointLifecycleService {
                generation_inputs: Arc::clone(&generation_inputs),
            }
        })
        .build()
        .expect("test OpenAI client");
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty tools");
    let (root, root_events) = Nanocodex::builder(openai)
        .tools(tools)
        .rollout(config.clone())
        .build()
        .expect("root agent with rollout");
    root.prompt("durable parent boundary")
        .await
        .expect("root turn")
        .result()
        .await
        .expect("completed root turn");
    let before = config
        .list_sessions()
        .expect("list root rollout")
        .into_iter()
        .map(|session| session.thread_id().to_owned())
        .collect::<Vec<_>>();

    assert!(matches!(
        root.fork().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    let after = config
        .list_sessions()
        .expect("list public rollouts after failed fork")
        .into_iter()
        .map(|session| session.thread_id().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(after, before);

    root.shutdown().await.expect("shutdown root");
    drop(root_events);
}

#[tokio::test]
async fn compacted_parent_fork_runs_and_resumes_from_the_child_rollout() {
    let home = tempdir().expect("temporary rollout home");
    let generation_inputs = Arc::new(Mutex::new(Vec::new()));
    let openai = || {
        let generation_inputs = Arc::clone(&generation_inputs);
        OpenAi::builder("test")
            .service(move || CheckpointLifecycleService {
                generation_inputs: Arc::clone(&generation_inputs),
            })
            .build()
            .expect("test OpenAI client")
    };
    let root_id = "019c0d31-c308-7d91-bff4-5dca82d15ac6";
    let root_tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty root tools");
    let (root, root_events) = Nanocodex::builder(openai())
        .tools(root_tools)
        .session_id(root_id.parse().expect("valid root session ID"))
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("root agent with rollout");
    root.prompt("parent durable boundary")
        .await
        .expect("parent turn")
        .result()
        .await
        .expect("completed parent turn");
    root.compact().await.expect("durable parent compaction");

    let (child, child_events) = root.fork().await.expect("durable fork");
    let child_session_id = child.session_id().to_string();
    let child_rollout = child
        .rollout()
        .expect("fork records its own rollout")
        .path()
        .to_path_buf();
    child
        .prompt("child continuation")
        .await
        .expect("child turn")
        .result()
        .await
        .expect("completed child turn");
    let child_records = std::fs::read_to_string(&child_rollout)
        .expect("read child rollout")
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<serde_json::Result<Vec<_>>>()
        .expect("decode child rollout");
    assert_eq!(
        child_records
            .iter()
            .filter(|record| {
                record["type"] == "response_item"
                    && record["payload"]["type"] == "compaction"
                    && record["payload"]["encrypted_content"] == "opaque-summary"
            })
            .count(),
        1
    );
    assert_eq!(
        child_records
            .iter()
            .filter(|record| {
                record["type"] == "event_msg"
                    && record["payload"]["type"] == "user_message"
                    && record["payload"]["message"] == "child continuation"
            })
            .count(),
        1
    );
    child.shutdown().await.expect("shutdown child");
    drop((child, child_events));

    let durable = RolloutConfig::new(home.path())
        .load_session(&child_session_id)
        .expect("child rollout remains resumable after its first turn");
    let (thread_id, snapshot, rollout) = durable.into_parts();
    assert_eq!(thread_id, child_session_id);
    let resumed_tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty resumed tools");
    let (resumed, resumed_events) = Nanocodex::builder(openai())
        .tools(resumed_tools)
        .session_id(thread_id.parse().expect("valid child session ID"))
        .resume(snapshot)
        .rollout(rollout)
        .build()
        .expect("resume child from its own rollout");
    resumed
        .prompt("resumed child continuation")
        .await
        .expect("resumed child turn")
        .result()
        .await
        .expect("completed resumed child turn");
    let resumed_records = std::fs::read_to_string(&child_rollout)
        .expect("read resumed rollout")
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<serde_json::Result<Vec<_>>>()
        .expect("decode resumed rollout");
    assert_eq!(
        resumed_records
            .iter()
            .filter(|record| {
                record["type"] == "response_item"
                    && record["payload"]["type"] == "compaction"
                    && record["payload"]["encrypted_content"] == "opaque-summary"
            })
            .count(),
        1
    );
    assert_eq!(
        resumed_records
            .iter()
            .filter(|record| {
                record["type"] == "event_msg"
                    && record["payload"]["type"] == "user_message"
                    && record["payload"]["message"] == "child continuation"
            })
            .count(),
        1
    );
    assert_eq!(
        resumed_records
            .iter()
            .filter(|record| {
                record["type"] == "event_msg"
                    && record["payload"]["type"] == "user_message"
                    && record["payload"]["message"] == "resumed child continuation"
            })
            .count(),
        1
    );
    resumed.shutdown().await.expect("shutdown resumed child");
    root.shutdown().await.expect("shutdown root");
    drop((resumed, resumed_events, root, root_events));

    let generations = generation_inputs.lock().expect("generation input lock");
    assert_eq!(generations.len(), 3);
    let child_input = serde_json::to_string(&generations[1]).expect("encode child request");
    assert_eq!(child_input.matches("child continuation").count(), 1);
    assert!(child_input.contains("opaque-summary"));
    let resumed_input = serde_json::to_string(&generations[2]).expect("encode resumed request");
    assert_eq!(
        resumed_input.matches("resumed child continuation").count(),
        1
    );
    assert!(resumed_input.contains("opaque-summary"));
}
