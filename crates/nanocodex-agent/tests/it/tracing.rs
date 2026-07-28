use std::{
    collections::HashMap,
    future::{Pending, Ready, pending, ready},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    task::{Context, Poll},
    time::Instant,
};

use nanocodex_agent::{
    Nanocodex, NanocodexError, OpenAi, Tools,
    transport::{ResponsesAttempt, ResponsesAttemptKind, ResponsesServiceResponse},
};
use nanocodex_oai_api::{
    responses::WarmupResponse,
    tower::{CodeCall, CodeCallKind, GenerationOutput, ResponsePipelineStats, ResponsesOutput},
};
use nanocodex_tools::{
    Tool, ToolContext, ToolDefinition, ToolInput, ToolResult, contract::async_trait,
};
use serde_json::json;
use tokio::sync::mpsc;
use tower::Service;
use tracing::{
    Id, Instrument, Subscriber,
    field::{Field, Visit},
    info_span,
    span::{Attributes, Record},
};
use tracing_subscriber::{Layer, layer::Context as LayerContext, prelude::*, registry::LookupSpan};

#[derive(Clone)]
struct PendingService;

impl Service<ResponsesAttempt> for PendingService {
    type Response = ResponsesServiceResponse;
    type Error = NanocodexError;
    type Future = Pending<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
        pending()
    }
}

struct PendingSpanTool {
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Tool for PendingSpanTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "trace__pending",
            "Remains active until the turn is cancelled.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, _input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        self.started.notify_one();
        std::future::pending().await
    }
}

#[derive(Clone)]
struct PendingToolService {
    calls: Arc<AtomicU32>,
}

impl Service<ResponsesAttempt> for PendingToolService {
    type Response = ResponsesServiceResponse;
    type Error = NanocodexError;
    type Future = Ready<Result<Self::Response, Self::Error>>;

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
            (1, ResponsesAttemptKind::Generation) => pending_tool_generation(),
            _ => panic!("unexpected attempt {call}: {:?}", request.kind()),
        };
        ready(Ok(ResponsesServiceResponse::new(output)))
    }
}

fn pending_tool_generation() -> ResponsesOutput {
    let item = serde_json::from_value(json!({
        "type": "function_call",
        "call_id": "call-pending",
        "namespace": "trace__",
        "name": "pending",
        "arguments": "{}"
    }))
    .expect("function call item decodes");
    ResponsesOutput::Generation(GenerationOutput {
        id: "resp-tool".to_owned(),
        status: "completed".to_owned(),
        end_turn: Some(false),
        final_message: None,
        output_items: vec![item],
        code_calls: vec![CodeCall {
            call_id: "call-pending".to_owned(),
            name: "pending".to_owned(),
            namespace: Some("trace__".to_owned()),
            input: "{}".to_owned(),
            kind: CodeCallKind::Function,
        }],
        usage: None,
        time_to_first_event_ns: 0,
        time_to_first_output_ns: None,
        pipeline_stats: ResponsePipelineStats::default(),
    })
}

#[derive(Clone, Default)]
struct TraceCapture(Arc<Mutex<HashMap<u64, CapturedSpan>>>);

#[derive(Clone)]
struct CapturedSpan {
    name: &'static str,
    parent: Option<u64>,
    opened: Instant,
    closed: Option<Instant>,
    fields: HashMap<String, String>,
}

struct FieldCapture<'a>(&'a mut HashMap<String, String>);

impl Visit for FieldCapture<'_> {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl<S> Layer<S> for TraceCapture
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: LayerContext<'_, S>) {
        let mut fields = HashMap::new();
        attributes.record(&mut FieldCapture(&mut fields));
        let parent = attributes
            .parent()
            .map(|parent| parent.clone().into_u64())
            .or_else(|| {
                attributes
                    .is_contextual()
                    .then(|| context.current_span().id().map(Id::into_u64))
                    .flatten()
            });
        self.0.lock().unwrap().insert(
            id.clone().into_u64(),
            CapturedSpan {
                name: attributes.metadata().name(),
                parent,
                opened: Instant::now(),
                closed: None,
                fields,
            },
        );
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _context: LayerContext<'_, S>) {
        if let Some(span) = self.0.lock().unwrap().get_mut(&id.clone().into_u64()) {
            values.record(&mut FieldCapture(&mut span.fields));
        }
    }

    fn on_close(&self, id: Id, _context: LayerContext<'_, S>) {
        if let Some(span) = self.0.lock().unwrap().get_mut(&id.into_u64()) {
            span.closed = Some(Instant::now());
        }
    }
}

#[test]
fn contextual_child_turns_preserve_parallel_orchestration_parentage() {
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let dispatch = tracing::Dispatch::new(subscriber);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    tracing::dispatcher::with_default(&dispatch, || {
        runtime.block_on(async {
            let (handles, mut received_handles) = mpsc::unbounded_channel();
            let openai = OpenAi::builder("test")
                .service(|| PendingService)
                .build()
                .unwrap();
            let (root, root_events) = Nanocodex::builder(openai)
                .tools_factory(move |handle| {
                    drop(handles.send(handle));
                    Tools::builder().without_defaults().build()
                })
                .build()
                .unwrap();
            let root_handle = received_handles.recv().await.unwrap();
            let (child_a, first_events) = root_handle.spawn().await.unwrap();
            let (child_b, second_events) = root_handle.spawn().await.unwrap();
            let (controls, mut received_controls) = mpsc::unbounded_channel();

            let (task_a, task_b) = async {
                let controls_a = controls.clone();
                let task_a = tokio::spawn(
                    async move {
                        let turn = child_a.prompt("child a").await.unwrap();
                        controls_a.send(turn.control()).unwrap();
                        assert!(matches!(
                            turn.result().await,
                            Err(NanocodexError::TurnCancelled)
                        ));
                    }
                    .instrument(info_span!("test.spawn_agent", child = "a")),
                );
                let task_b = tokio::spawn(
                    async move {
                        let turn = child_b.prompt("child b").await.unwrap();
                        controls.send(turn.control()).unwrap();
                        assert!(matches!(
                            turn.result().await,
                            Err(NanocodexError::TurnCancelled)
                        ));
                    }
                    .instrument(info_span!("test.spawn_agent", child = "b")),
                );
                (task_a, task_b)
            }
            .instrument(info_span!("test.code_mode.cell"))
            .await;

            let control_a = received_controls.recv().await.unwrap();
            let control_b = received_controls.recv().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let (cancel_a, cancel_b) = tokio::join!(control_a.cancel(), control_b.cancel());
            cancel_a.unwrap();
            cancel_b.unwrap();
            task_a.await.unwrap();
            task_b.await.unwrap();

            let openai = OpenAi::builder("test")
                .service(|| PendingService)
                .build()
                .unwrap();
            let (plain, plain_events) = Nanocodex::builder(openai).build().unwrap();
            let plain_turn = plain.prompt("plain root turn").await.unwrap();
            plain_turn.cancel().await.unwrap();
            assert!(matches!(
                plain_turn.result().await,
                Err(NanocodexError::TurnCancelled)
            ));

            drop((plain, plain_events, root, root_events));
            drop((first_events, second_events));
        });
    });

    let spans = capture.0.lock().unwrap();
    let turns = spans
        .iter()
        .filter(|(_, span)| span.name == "agent.turn")
        .collect::<Vec<_>>();
    assert_eq!(turns.len(), 3);

    let child_turns = turns
        .iter()
        .filter(|(_, span)| {
            span.parent
                .and_then(|parent| spans.get(&parent))
                .is_some_and(|parent| parent.name == "test.spawn_agent")
        })
        .map(|(_, span)| *span)
        .collect::<Vec<_>>();
    assert_eq!(child_turns.len(), 2);
    assert!(turns.iter().any(|(_, span)| span.parent.is_none()));

    let first = child_turns[0];
    let second = child_turns[1];
    assert!(
        first.opened < second.closed.unwrap() && second.opened < first.closed.unwrap(),
        "child turn intervals should overlap"
    );
}

#[test]
fn cancelled_tool_span_records_its_terminal_state_before_closing() {
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let dispatch = tracing::Dispatch::new(subscriber);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    tracing::dispatcher::with_default(&dispatch, || {
        runtime.block_on(async {
            let calls = Arc::new(AtomicU32::new(0));
            let started = Arc::new(tokio::sync::Notify::new());
            let service_calls = Arc::clone(&calls);
            let openai = OpenAi::builder("test")
                .service(move || PendingToolService {
                    calls: Arc::clone(&service_calls),
                })
                .build()
                .unwrap();
            let tools = Tools::builder()
                .without_defaults()
                .tool(PendingSpanTool {
                    started: Arc::clone(&started),
                })
                .build()
                .unwrap();
            let (agent, events) = Nanocodex::builder(openai).tools(tools).build().unwrap();
            let turn = agent.prompt("run pending tool").await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
                .await
                .expect("pending tool did not start");
            turn.cancel().await.unwrap();
            assert!(matches!(
                turn.result().await,
                Err(NanocodexError::TurnCancelled)
            ));
            agent.shutdown().await.unwrap();
            drop((agent, events));
            assert_eq!(calls.load(Ordering::Relaxed), 2);
        });
    });

    let spans = capture.0.lock().unwrap();
    let tool = spans
        .values()
        .find(|span| span.name == "tool.call")
        .expect("tool span was not captured");
    assert_eq!(
        tool.fields.get("status").map(String::as_str),
        Some("cancelled")
    );
    assert_eq!(
        tool.fields.get("otel.status_code").map(String::as_str),
        Some("ERROR")
    );
    assert!(
        tool.fields
            .get("duration_ns")
            .and_then(|duration| duration.parse::<u64>().ok())
            .is_some_and(|duration| duration > 0)
    );
    assert!(tool.closed.is_some(), "tool span did not close");
}
