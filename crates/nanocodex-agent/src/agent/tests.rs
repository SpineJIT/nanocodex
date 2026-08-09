use std::{
    future::{Future, ready},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use nanocodex_oai_api::{
    responses::{ContentItem, MessageRole, ResponseItem, Usage, WarmupResponse},
    tower::{GenerationOutput, ResponsePipelineStats, ResponsesAttemptKind, ResponsesOutput},
};
use tempfile::tempdir;
use tokio::sync::{mpsc, oneshot};
use tower::Service;

use super::*;
use crate::rollout::RolloutConfig;

#[derive(Clone)]
struct DelayedCompletedService {
    generation_started: mpsc::UnboundedSender<()>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    generation_calls: Arc<std::sync::atomic::AtomicUsize>,
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
    let (agent, events) = Nanocodex::builder(openai)
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
    agent.shutdown().await.expect("shutdown still cleans up");
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
