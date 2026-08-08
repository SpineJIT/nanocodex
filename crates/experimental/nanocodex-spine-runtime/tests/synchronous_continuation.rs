use std::{
    future::{Ready, ready},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    task::{Context, Poll},
};

use nanocodex::oai::{
    ResponseError,
    responses::{ContentItem, MessageRole, ResponseItem, Usage, WarmupResponse},
    tower::{
        CodeCall, CodeCallKind, GenerationOutput, ResponsePipelineStats, ResponsesAttempt,
        ResponsesAttemptKind, ResponsesOutput, ResponsesServiceResponse,
    },
};
use nanocodex::{Nanocodex, OpenAi, Thinking, Tools};
use nanocodex_spine_runtime::{SpineRuntime, SpineRuntimeLimits, with_spine_tools};
use tower::Service;

#[tokio::test]
async fn open_runs_a_child_then_returns_only_its_compact_handoff_to_the_parent() {
    let calls = Arc::new(AtomicU32::new(0));
    let service_calls = Arc::clone(&calls);
    let openai = OpenAi::builder("test-key")
        .service(move || ScriptedService {
            calls: Arc::clone(&service_calls),
            script: Script::Close,
        })
        .build()
        .unwrap();
    let runtime = Arc::new(SpineRuntime::new(SpineRuntimeLimits::default()));
    let tools = Tools::builder().without_defaults().build().unwrap();
    let runtime_for_tools = Arc::clone(&runtime);
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(".")
        .tools_factory(move |agent| {
            with_spine_tools(tools.clone(), agent, Arc::clone(&runtime_for_tools))
        })
        .build()
        .unwrap();

    let result = agent
        .prompt("Use a focused continuation to inspect the parser.")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();

    assert_eq!(result.final_message(), Some("parent resumed"));
    assert_eq!(runtime.projection().unwrap().cursor.to_string(), "1");
    assert_eq!(
        runtime.projection().unwrap().nodes[1]
            .memory
            .as_ref()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(calls.load(Ordering::Relaxed), 4);

    agent.shutdown().await.unwrap();
    drop(events);
}

#[tokio::test]
async fn next_runs_a_sibling_from_the_same_frozen_parent_before_resuming() {
    let calls = Arc::new(AtomicU32::new(0));
    let service_calls = Arc::clone(&calls);
    let openai = OpenAi::builder("test-key")
        .service(move || ScriptedService {
            calls: Arc::clone(&service_calls),
            script: Script::Next,
        })
        .build()
        .unwrap();
    let runtime = Arc::new(SpineRuntime::new(SpineRuntimeLimits::default()));
    let tools = Tools::builder().without_defaults().build().unwrap();
    let runtime_for_tools = Arc::clone(&runtime);
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(".")
        .tools_factory(move |agent| {
            with_spine_tools(tools.clone(), agent, Arc::clone(&runtime_for_tools))
        })
        .build()
        .unwrap();

    let result = agent
        .prompt("Use a focused continuation to inspect the parser.")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();

    assert_eq!(result.final_message(), Some("parent resumed"));
    assert_eq!(runtime.projection().unwrap().cursor.to_string(), "1");
    assert_eq!(
        runtime.projection().unwrap().nodes[1]
            .memory
            .as_ref()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        runtime.projection().unwrap().nodes[2]
            .memory
            .as_ref()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(calls.load(Ordering::Relaxed), 5);

    agent.shutdown().await.unwrap();
    drop(events);
}

#[tokio::test]
async fn failed_child_continuation_restores_the_parent_logical_tree() {
    let calls = Arc::new(AtomicU32::new(0));
    let service_calls = Arc::clone(&calls);
    let openai = OpenAi::builder("test-key")
        .service(move || ScriptedService {
            calls: Arc::clone(&service_calls),
            script: Script::ChildMessage,
        })
        .build()
        .unwrap();
    let runtime = Arc::new(SpineRuntime::new(SpineRuntimeLimits::default()));
    let tools = Tools::builder().without_defaults().build().unwrap();
    let runtime_for_tools = Arc::clone(&runtime);
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(".")
        .tools_factory(move |agent| {
            with_spine_tools(tools.clone(), agent, Arc::clone(&runtime_for_tools))
        })
        .build()
        .unwrap();

    let result = agent
        .prompt("Use a focused continuation to inspect the parser.")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();

    let projection = runtime.projection().unwrap();
    assert_eq!(result.final_message(), Some("parent recovered"));
    assert_eq!(projection.cursor.to_string(), "1");
    assert_eq!(projection.nodes.len(), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 4);

    agent.shutdown().await.unwrap();
    drop(events);
}

#[derive(Clone)]
struct ScriptedService {
    calls: Arc<AtomicU32>,
    script: Script,
}

#[derive(Clone, Copy)]
enum Script {
    Close,
    Next,
    ChildMessage,
}

impl Service<ResponsesAttempt> for ScriptedService {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = Ready<Result<ResponsesServiceResponse, ResponseError>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let output = match (self.script, call, request.kind()) {
            (_, 0, ResponsesAttemptKind::Warmup) => ResponsesOutput::Warmup(WarmupResponse {
                id: "resp-warmup".to_owned(),
                usage: None,
            }),
            (_, 1, ResponsesAttemptKind::Generation) => code_generation(
                "resp-root-open",
                "call-root-exec",
                "const handoff = await tools.spine__open({summary: 'inspect the parser'}); text(handoff.memory);",
            ),
            (Script::Close, 2, ResponsesAttemptKind::Generation) => code_generation(
                "resp-child-close",
                "call-child-exec",
                "await tools.spine__close({memory: 'parser accepts one token too eagerly'});",
            ),
            (Script::Close, 3, ResponsesAttemptKind::Generation) => {
                assert_input_contains(&request, "parser accepts one token too eagerly");
                final_generation("resp-parent-final", "parent resumed")
            }
            (Script::Next, 2, ResponsesAttemptKind::Generation) => code_generation(
                "resp-child-next",
                "call-child-exec",
                "await tools.spine__next({summary: 'confirm the fix', memory: 'parser requires one-token lookahead'});",
            ),
            (Script::Next, 3, ResponsesAttemptKind::Generation) => {
                assert_input_contains(&request, "parser requires one-token lookahead");
                code_generation(
                    "resp-sibling-close",
                    "call-sibling-exec",
                    "await tools.spine__close({memory: 'sibling confirmed the fix'});",
                )
            }
            (Script::Next, 4, ResponsesAttemptKind::Generation) => {
                assert_input_contains(&request, "sibling confirmed the fix");
                final_generation("resp-parent-final", "parent resumed")
            }
            (Script::ChildMessage, 2, ResponsesAttemptKind::Generation) => {
                final_generation("resp-child-message", "child cannot finish")
            }
            (Script::ChildMessage, 3, ResponsesAttemptKind::Generation) => {
                final_generation("resp-parent-final", "parent recovered")
            }
            _ => panic!("unexpected scripted Responses attempt {call}"),
        };
        ready(Ok(ResponsesServiceResponse::new(output)))
    }
}

fn assert_input_contains(request: &ResponsesAttempt, expected: &str) {
    let input = serde_json::to_string(&request.input_items().collect::<Vec<_>>()).unwrap();
    assert!(
        input.contains(expected),
        "request did not contain expected Spine handoff: {input}"
    );
}

fn code_generation(response_id: &str, call_id: &str, source: &str) -> ResponsesOutput {
    let output_item = serde_json::from_value(serde_json::json!({
        "type": "custom_tool_call",
        "call_id": call_id,
        "name": "exec",
        "input": source,
    }))
    .unwrap();
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
