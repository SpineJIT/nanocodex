use std::{
    future::{Future, ready},
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use nanocodex_oai_api::{
    ResponseError,
    events::{AgentEventData, AgentEventKind, RunEvent, RunStatus},
    responses::{ContentItem, MessageRole, ResponseItem, ResponseItemId, Usage, WarmupResponse},
    tower::{
        CompactionOutput, GenerationOutput, ResponsePipelineStats, ResponsesAttemptKind,
        ResponsesOutput,
    },
    transport::ResponsesError,
};
use nanocodex_tools::{
    Tool, ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, ToolTurnBehavior,
    contract::async_trait,
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
    generation_requests: Arc<Mutex<Vec<GenerationRequest>>>,
}

struct GenerationRequest {
    session_id: String,
    prompt_cache_key: String,
    prefix: Vec<ResponseItem>,
    input: Vec<ResponseItem>,
}

struct FinishTurnTool {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Tool for FinishTurnTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "finish_turn",
            "Ends the enclosing turn after the Code Mode cell commits.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    fn turn_behavior(&self) -> ToolTurnBehavior {
        ToolTurnBehavior::FinishTurnOnSuccess
    }

    async fn execute(&self, _input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        use std::sync::atomic::Ordering;

        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ToolOutput::text("terminal result"))
    }
}

#[derive(Clone)]
struct TerminalToolService;

#[derive(Clone, Copy)]
enum FailingAttempt {
    Generation,
    Compaction,
}

#[derive(Clone, Copy)]
struct FailingAttemptService {
    attempt: FailingAttempt,
}

impl Service<ResponsesAttempt> for FailingAttemptService {
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
        let fails = matches!(
            (self.attempt, request.kind()),
            (FailingAttempt::Generation, ResponsesAttemptKind::Generation)
                | (FailingAttempt::Compaction, ResponsesAttemptKind::Compaction)
        );
        if fails {
            return Box::pin(ready(Err(ResponseError::from(
                ResponsesError::UnexpectedEnd,
            ))));
        }
        let output = match request.kind() {
            ResponsesAttemptKind::Warmup => ResponsesOutput::Warmup(WarmupResponse {
                id: "resp-warmup".to_owned(),
                usage: None,
            }),
            _ => panic!("failing-attempt test received an unsupported attempt"),
        };
        Box::pin(ready(Ok(ResponsesServiceResponse::new(output))))
    }
}

impl Service<ResponsesAttempt> for TerminalToolService {
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
            ResponsesAttemptKind::Generation => {
                let input = "await tools.finish_turn({});";
                let output_item = serde_json::from_value(serde_json::json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": input,
                }))
                .expect("terminal Code Mode call decodes");
                ResponsesOutput::Generation(GenerationOutput {
                    id: "resp-terminal".to_owned(),
                    status: "completed".to_owned(),
                    end_turn: Some(false),
                    final_message: None,
                    output_items: vec![output_item],
                    code_calls: vec![nanocodex_oai_api::tower::CodeCall {
                        call_id: "call-exec".to_owned(),
                        name: "exec".to_owned(),
                        namespace: None,
                        input: input.to_owned(),
                        kind: nanocodex_oai_api::tower::CodeCallKind::Custom,
                    }],
                    usage: None,
                    time_to_first_event_ns: 0,
                    time_to_first_output_ns: None,
                    pipeline_stats: ResponsePipelineStats::default(),
                })
            }
            _ => panic!("terminal durability test received an unsupported attempt"),
        };
        Box::pin(ready(Ok(ResponsesServiceResponse::new(output))))
    }
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
                let profile = nanocodex_oai_api::__private::test_support::request_profile(&request);
                self.generation_requests
                    .lock()
                    .expect("generation request lock")
                    .push(GenerationRequest {
                        session_id: profile.session_id().to_owned(),
                        prompt_cache_key: profile.prompt_cache_key().to_owned(),
                        prefix: profile.prefix().to_vec(),
                        input: request.input_items().cloned().collect(),
                    });
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
                let output = || {
                    ResponsesServiceResponse::new(ResponsesOutput::Compaction(CompactionOutput {
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
                    }))
                };
                let release = self.release.lock().expect("release lock").take();
                let Some(release) = release else {
                    return Box::pin(ready(Ok(output())));
                };
                self.compaction_started
                    .send(())
                    .expect("test observes the delayed compaction attempt");
                Box::pin(async move {
                    release.await.expect("test releases compaction");
                    Ok(output())
                })
            }
            ResponsesAttemptKind::Warmup => Box::pin(ready(Ok(ResponsesServiceResponse::new(
                ResponsesOutput::Warmup(WarmupResponse {
                    id: "resp-warmup".to_owned(),
                    usage: None,
                }),
            )))),
            ResponsesAttemptKind::Generation => Box::pin(ready(Ok(ResponsesServiceResponse::new(
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
async fn run_completed_event_makes_the_rollout_immediately_resumable() {
    let home = tempdir().expect("temporary rollout home");
    let generation_requests = Arc::new(Mutex::new(Vec::new()));
    let openai = OpenAi::builder("test")
        .service({
            let generation_requests = Arc::clone(&generation_requests);
            move || CheckpointLifecycleService {
                generation_requests: Arc::clone(&generation_requests),
            }
        })
        .build()
        .expect("test OpenAI client");
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty tools");
    let session_id = "019c0d31-c308-7d91-bff4-5dca82d15ac6";
    let (agent, mut events) = Nanocodex::builder(openai)
        .tools(tools)
        .session_id(session_id.parse().expect("valid session ID"))
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("agent with rollout");
    let turn = agent.prompt("durable event boundary").await.expect("turn");

    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("turn emits a terminal event")
            .expect("event stream remains open");
        if event.kind == AgentEventKind::RunCompleted {
            break;
        }
    }

    let durable = RolloutConfig::new(home.path())
        .load_session(session_id)
        .expect("RunCompleted exposes a resumable rollout");
    assert!(
        serde_json::to_value(durable.snapshot()).expect("encode durable snapshot")["history"]
            .to_string()
            .contains("durable event boundary")
    );
    turn.result().await.expect("turn result remains available");

    agent.shutdown().await.expect("shutdown agent");
}

#[tokio::test]
async fn active_cancellation_persistence_failure_fails_the_turn_once() {
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
    agent.durability.inject_write_failures(1).await;

    let turn = agent.prompt("cancelled durable turn").await.expect("turn");
    let control = turn.control();
    generation_started_rx
        .recv()
        .await
        .expect("generation starts");

    assert!(matches!(
        control.cancel().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        turn.result().await,
        Err(NanocodexError::PersistRollout { .. })
    ));

    let mut completed = 0;
    let mut failed = 0;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("active cancellation emits a terminal event")
            .expect("agent event stream remains open");
        match event.kind {
            AgentEventKind::RunCompleted => completed += 1,
            AgentEventKind::RunFailed => {
                failed += 1;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(completed, 0);
    assert_eq!(failed, 1);
    assert_eq!(generation_calls.load(Ordering::Relaxed), 1);
    assert!(matches!(
        agent.shutdown().await,
        Err(NanocodexError::Shutdown(error))
            if matches!(error.as_ref(), NanocodexError::PersistRollout { .. })
    ));
    drop(release);
}

#[tokio::test]
async fn terminal_tool_persistence_failure_fails_the_turn() {
    use std::sync::atomic::Ordering;

    let home = tempdir().expect("temporary rollout home");
    let workspace = tempdir().expect("temporary workspace");
    let tool_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let openai = OpenAi::builder("test")
        .service(|| TerminalToolService)
        .build()
        .expect("test OpenAI client");
    let tools = Tools::builder()
        .without_defaults()
        .tool(FinishTurnTool {
            calls: Arc::clone(&tool_calls),
        })
        .build()
        .expect("terminal tool");
    let (agent, mut events) = Nanocodex::builder(openai)
        .tools(tools)
        .workspace(workspace.path())
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("agent with rollout");
    agent.durability.inject_write_failures(1).await;

    let result = agent
        .prompt("finish through the application tool")
        .await
        .expect("accepted turn")
        .result()
        .await;

    assert!(matches!(result, Err(NanocodexError::PersistRollout { .. })));
    assert_eq!(tool_calls.load(Ordering::Relaxed), 1);

    let mut completed = 0;
    let mut failed = 0;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("terminal tool emits a terminal event")
            .expect("agent event stream remains open");
        match event.kind {
            AgentEventKind::RunCompleted => completed += 1,
            AgentEventKind::RunFailed => {
                failed += 1;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(completed, 0);
    assert_eq!(failed, 1);
    assert!(matches!(
        agent.prompt("must not run after failed durability").await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.shutdown().await,
        Err(NanocodexError::Shutdown(error))
            if matches!(error.as_ref(), NanocodexError::PersistRollout { .. })
    ));
}

#[tokio::test]
async fn failed_provider_turn_persistence_failure_fail_stops_the_agent() {
    let home = tempdir().expect("temporary rollout home");
    let openai = OpenAi::builder("test")
        .service(|| FailingAttemptService {
            attempt: FailingAttempt::Generation,
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
    agent.durability.inject_write_failures(1).await;

    let result = agent
        .prompt("provider failure must still persist its safe boundary")
        .await
        .expect("accepted turn")
        .result()
        .await;

    assert!(matches!(result, Err(NanocodexError::PersistRollout { .. })));

    let mut model_failed = false;
    let mut completed = 0;
    let mut failed = 0;
    while failed == 0 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("failed provider turn emits lifecycle events")
            .expect("agent event stream remains open");
        match event.kind {
            AgentEventKind::ModelCallFailed => model_failed = true,
            AgentEventKind::RunCompleted => completed += 1,
            AgentEventKind::RunFailed => failed += 1,
            _ => {}
        }
    }
    assert!(
        model_failed,
        "the provider failure must reach the model lifecycle"
    );
    assert_eq!(completed, 0);
    assert_eq!(failed, 1);
    assert!(matches!(
        agent.prompt("must not run after failed durability").await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.shutdown().await,
        Err(NanocodexError::Shutdown(error))
            if matches!(error.as_ref(), NanocodexError::PersistRollout { .. })
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
    let tool_handle = Arc::new(Mutex::new(None));
    let (agent, mut events) = Nanocodex::builder(openai)
        .tools_factory({
            let tool_handle = Arc::clone(&tool_handle);
            move |handle| {
                *tool_handle.lock().expect("tool handle lock") = Some(handle);
                Tools::builder().without_defaults().build()
            }
        })
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("agent with rollout");
    let tool_handle = tool_handle
        .lock()
        .expect("tool handle lock")
        .clone()
        .expect("tools factory receives an agent handle");
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
        tool_handle.spawn().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        tool_handle.fork().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.compact().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert_eq!(generation_calls.load(Ordering::Relaxed), 1);

    let mut terminal_statuses = Vec::new();
    let mut failure_messages = Vec::new();
    while terminal_statuses.len() < 2 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("every accepted turn emits a terminal event")
            .expect("agent event stream remains open");
        if let AgentEventData::Run(RunEvent::Error(error)) =
            event.data().expect("decode run error event")
        {
            failure_messages.push(error.message);
        }
        if let AgentEventData::Run(RunEvent::Failed(terminal)) =
            event.data().expect("decode run terminal event")
        {
            terminal_statuses.push(terminal.status);
        }
    }
    assert_eq!(
        terminal_statuses,
        vec![RunStatus::Failed, RunStatus::Failed]
    );
    assert_eq!(failure_messages.len(), 2);
    assert!(
        failure_messages
            .iter()
            .all(|message| message.contains("failed to persist Codex rollout"))
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
    let generation_requests = Arc::new(Mutex::new(Vec::new()));
    let openai = OpenAi::builder("test")
        .service({
            let generation_requests = Arc::clone(&generation_requests);
            move || CheckpointLifecycleService {
                generation_requests: Arc::clone(&generation_requests),
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
        generation_requests
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
async fn rollout_flush_waits_for_an_active_turns_durable_boundary() {
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
    let turn = agent.prompt("durable turn").await.expect("accepted turn");
    generation_started_rx
        .recv()
        .await
        .expect("generation starts");
    let first_flush_agent = agent.clone();
    let mut first_flush = tokio::spawn(async move { first_flush_agent.flush_rollout().await });
    let second_flush_agent = agent.clone();
    let mut second_flush = tokio::spawn(async move { second_flush_agent.flush_rollout().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut first_flush)
            .await
            .is_err(),
        "first flush must wait for the active turn's durability boundary"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut second_flush)
            .await
            .is_err(),
        "second flush must wait for the active turn's durability boundary"
    );
    release.send(()).expect("release generation");
    turn.result().await.expect("durable completed turn");
    first_flush
        .await
        .expect("flush task joins")
        .expect("flush succeeds after the durable boundary");
    second_flush
        .await
        .expect("second flush task joins")
        .expect("both flush callers succeed after the durable boundary");
    drop(events);
}

#[tokio::test]
async fn rollout_flush_succeeds_after_a_durable_compaction() {
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
    agent
        .prompt("durable context before compaction")
        .await
        .expect("pre-compaction turn")
        .result()
        .await
        .expect("durable pre-compaction turn");

    let compact_agent = agent.clone();
    let compact = tokio::spawn(async move { compact_agent.compact().await });
    compaction_started_rx
        .recv()
        .await
        .expect("compaction starts");
    let flush_agent = agent.clone();
    let mut flush = tokio::spawn(async move { flush_agent.flush_rollout().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut flush)
            .await
            .is_err(),
        "flush waits for the active compaction's durable boundary"
    );
    release.send(()).expect("release compaction");
    compact
        .await
        .expect("compaction task joins")
        .expect("durable compaction");
    flush
        .await
        .expect("flush task joins")
        .expect("flush succeeds after durable compaction");

    let durable = RolloutConfig::new(home.path())
        .load_session(&agent.session_id().to_string())
        .expect("successful compaction makes the rollout immediately readable");
    assert!(
        serde_json::to_value(durable.snapshot())
            .expect("encode durable compaction snapshot")["history"]
            .to_string()
            .contains("opaque-summary")
    );
    assert_eq!(
        compaction_calls.load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    agent.shutdown().await.expect("shutdown agent");
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
    let flush_agent = agent.clone();
    let mut flush = tokio::spawn(async move { flush_agent.flush_rollout().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut flush)
            .await
            .is_err(),
        "flush must wait for the active compaction durability boundary"
    );
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
    assert!(matches!(
        flush.await.expect("flush task joins"),
        Err(NanocodexError::PersistRollout { .. })
    ));
    drop(events);
}

#[tokio::test]
async fn cancelled_compaction_persistence_failure_fail_stops_replacement() {
    use std::sync::atomic::Ordering;

    let home = tempdir().expect("temporary rollout home");
    let (compaction_started, mut compaction_started_rx) = mpsc::unbounded_channel();
    let (_release, release_rx) = oneshot::channel();
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
    agent.durability.inject_write_failures(1).await;

    let first_agent = agent.clone();
    let first = tokio::spawn(async move { first_agent.compact().await });
    compaction_started_rx
        .recv()
        .await
        .expect("first compaction reaches the provider");
    let second_agent = agent.clone();
    let second = tokio::spawn(async move { second_agent.compact().await });

    assert!(matches!(
        first.await.expect("cancelled compaction task joins"),
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        second.await.expect("replacement compaction task joins"),
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert_eq!(compaction_calls.load(Ordering::Relaxed), 1);
    assert!(matches!(
        agent.compact().await,
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
async fn failed_provider_compaction_persistence_failure_fail_stops_the_agent() {
    let home = tempdir().expect("temporary rollout home");
    let openai = OpenAi::builder("test")
        .service(|| FailingAttemptService {
            attempt: FailingAttempt::Compaction,
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
        agent.compact().await,
        Err(NanocodexError::PersistRollout { .. })
    ));
    assert!(matches!(
        agent.prompt("must not run after failed durability").await,
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
    let generation_requests = Arc::new(Mutex::new(Vec::new()));
    let openai = || {
        let generation_requests = Arc::clone(&generation_requests);
        OpenAi::builder("test")
            .service(move || CheckpointLifecycleService {
                generation_requests: Arc::clone(&generation_requests),
            })
            .build()
            .expect("test OpenAI client")
    };
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty tools");
    let root_session_id = "019c0d31-c308-7d91-bff4-5dca82d15ac6";
    let (root, root_events) = Nanocodex::builder(openai())
        .tools(tools)
        .session_id(root_session_id.parse().expect("valid root session ID"))
        .prompt_cache_key("durable-root-cache-key")
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("root agent with rollout");
    root.prompt("durable parent boundary")
        .await
        .expect("root turn")
        .result()
        .await
        .expect("turn result establishes a durable rollout boundary");

    let persisted_parent = RolloutConfig::new(home.path())
        .load_session(root_session_id)
        .expect("turn result makes the parent rollout immediately resumable");
    assert!(
        serde_json::to_value(persisted_parent.snapshot()).expect("encode durable parent snapshot")
            ["history"]
            .to_string()
            .contains("durable parent boundary")
    );
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
        "durable-root-cache-key"
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
    assert_eq!(snapshot["prompt_cache_key"], "durable-root-cache-key");
    assert!(
        snapshot["history"]
            .to_string()
            .contains("durable parent boundary")
    );

    child
        .prompt("live child continuation")
        .await
        .expect("live child turn")
        .result()
        .await
        .expect("completed live child turn");

    child.shutdown().await.expect("shutdown child");
    drop((child, child_events));

    let durable = RolloutConfig::new(home.path())
        .load_session(&child_session_id)
        .expect("child rollout remains resumable after shutdown");
    let (thread_id, snapshot, rollout) = durable.into_parts();
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
        .expect("resume fork from its own rollout");
    resumed
        .prompt("resumed child continuation")
        .await
        .expect("resumed child turn")
        .result()
        .await
        .expect("completed resumed child turn");

    resumed.shutdown().await.expect("shutdown resumed child");
    root.shutdown().await.expect("shutdown root");
    let generation_requests = generation_requests.lock().expect("generation request lock");
    assert_eq!(generation_requests.len(), 3);
    let live_child_request = &generation_requests[1];
    let resumed_child_request = &generation_requests[2];
    assert_eq!(live_child_request.session_id, child_session_id);
    assert_eq!(resumed_child_request.session_id, child_session_id);
    assert_eq!(
        live_child_request.prompt_cache_key,
        "durable-root-cache-key"
    );
    assert_eq!(
        resumed_child_request.prompt_cache_key,
        "durable-root-cache-key"
    );
    assert_eq!(
        serde_json::to_vec(&live_child_request.prefix).expect("encode live child prefix"),
        serde_json::to_vec(&resumed_child_request.prefix).expect("encode resumed child prefix")
    );
    assert!(
        serde_json::to_string(&resumed_child_request.input)
            .expect("encode resumed child input")
            .contains("live child continuation")
    );
    drop((resumed, resumed_events, root_events));
}

#[tokio::test]
async fn nested_fork_seeds_a_grandchild_rollout_before_its_first_prompt() {
    let home = tempdir().expect("temporary rollout home");
    let generation_requests = Arc::new(Mutex::new(Vec::new()));
    let openai = || {
        let generation_requests = Arc::clone(&generation_requests);
        OpenAi::builder("test")
            .service(move || CheckpointLifecycleService {
                generation_requests: Arc::clone(&generation_requests),
            })
            .build()
            .expect("test OpenAI client")
    };
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty tools");
    let root_session_id = "019c0d31-c308-7d91-bff4-5dca82d15ac6";
    let (root, root_events) = Nanocodex::builder(openai())
        .tools(tools)
        .session_id(root_session_id.parse().expect("valid root session ID"))
        .prompt_cache_key("durable-root-cache-key")
        .rollout(RolloutConfig::new(home.path()))
        .build()
        .expect("root agent with rollout");
    root.prompt("durable root boundary")
        .await
        .expect("root turn")
        .result()
        .await
        .expect("completed root turn");

    let (child, child_events) = root.fork().await.expect("durable child fork");
    child
        .prompt("durable child boundary")
        .await
        .expect("child turn")
        .result()
        .await
        .expect("completed child turn");
    let (grandchild, grandchild_events) = child.fork().await.expect("durable grandchild fork");
    let grandchild_session_id = grandchild.session_id().to_string();
    let grandchild_rollout = grandchild
        .rollout()
        .expect("grandchild records its own rollout")
        .path()
        .to_path_buf();

    grandchild
        .shutdown()
        .await
        .expect("shutdown unprompted grandchild");
    drop((grandchild, grandchild_events));

    let durable = RolloutConfig::new(home.path())
        .load_session(&grandchild_session_id)
        .expect("grandchild rollout is resumable before its first prompt");
    let snapshot = serde_json::to_value(durable.snapshot()).expect("encode grandchild snapshot");
    assert_eq!(snapshot["lineage_id"], root_session_id);
    assert_eq!(snapshot["prompt_cache_key"], "durable-root-cache-key");
    assert!(
        snapshot["history"]
            .to_string()
            .contains("durable root boundary")
    );
    assert!(
        snapshot["history"]
            .to_string()
            .contains("durable child boundary")
    );
    let seeded_grandchild_lines = std::fs::read_to_string(&grandchild_rollout)
        .expect("read seeded grandchild rollout")
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<serde_json::Result<Vec<_>>>()
        .expect("decode seeded grandchild rollout");
    assert!(
        !seeded_grandchild_lines
            .iter()
            .any(|line| line["type"] == "event_msg"),
        "the seed must not manufacture an unprompted grandchild task"
    );

    let (thread_id, snapshot, rollout) = durable.into_parts();
    let resumed_tools = Tools::builder()
        .without_defaults()
        .build()
        .expect("empty resumed tools");
    let (resumed, resumed_events) = Nanocodex::builder(openai())
        .tools(resumed_tools)
        .session_id(thread_id.parse().expect("valid grandchild session ID"))
        .resume(snapshot)
        .rollout(rollout)
        .build()
        .expect("resume grandchild from its own rollout");
    resumed
        .prompt("resumed grandchild continuation")
        .await
        .expect("resumed grandchild turn")
        .result()
        .await
        .expect("completed resumed grandchild turn");

    resumed
        .shutdown()
        .await
        .expect("shutdown resumed grandchild");
    child.shutdown().await.expect("shutdown child");
    root.shutdown().await.expect("shutdown root");
    let generation_requests = generation_requests.lock().expect("generation request lock");
    let resumed_grandchild_request = generation_requests
        .iter()
        .find(|request| request.session_id == grandchild_session_id)
        .expect("resumed grandchild request");
    assert_eq!(
        resumed_grandchild_request.prompt_cache_key,
        "durable-root-cache-key"
    );
    assert!(
        serde_json::to_string(&resumed_grandchild_request.input)
            .expect("encode resumed grandchild input")
            .contains("durable child boundary")
    );
    drop((
        resumed,
        resumed_events,
        child,
        child_events,
        root,
        root_events,
    ));
}

async fn assert_failed_public_fork_does_not_publish_a_child_rollout(config: RolloutConfig) {
    let generation_requests = Arc::new(Mutex::new(Vec::new()));
    let openai = OpenAi::builder("test")
        .service({
            let generation_requests = Arc::clone(&generation_requests);
            move || CheckpointLifecycleService {
                generation_requests: Arc::clone(&generation_requests),
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
async fn failed_public_fork_seed_does_not_publish_a_child_rollout() {
    let home = tempdir().expect("temporary rollout home");
    assert_failed_public_fork_does_not_publish_a_child_rollout(
        RolloutConfig::new(home.path()).fail_fork_seed_for_test(),
    )
    .await;
}

#[tokio::test]
async fn failed_public_fork_publish_does_not_publish_a_child_rollout() {
    let home = tempdir().expect("temporary rollout home");
    assert_failed_public_fork_does_not_publish_a_child_rollout(
        RolloutConfig::new(home.path()).fail_fork_publish_for_test(),
    )
    .await;
}

#[tokio::test]
async fn compacted_parent_fork_runs_and_resumes_from_the_child_rollout() {
    let home = tempdir().expect("temporary rollout home");
    let generation_requests = Arc::new(Mutex::new(Vec::new()));
    let openai = || {
        let generation_requests = Arc::clone(&generation_requests);
        OpenAi::builder("test")
            .service(move || CheckpointLifecycleService {
                generation_requests: Arc::clone(&generation_requests),
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

    let generations = generation_requests.lock().expect("generation request lock");
    assert_eq!(generations.len(), 3);
    let child_input = serde_json::to_string(&generations[1].input).expect("encode child request");
    assert_eq!(child_input.matches("child continuation").count(), 1);
    assert!(child_input.contains("opaque-summary"));
    let resumed_input =
        serde_json::to_string(&generations[2].input).expect("encode resumed request");
    assert_eq!(
        resumed_input.matches("resumed child continuation").count(),
        1
    );
    assert!(resumed_input.contains("opaque-summary"));
}
