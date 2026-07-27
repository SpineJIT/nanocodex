use std::{
    error::Error as _,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    task::Poll,
};

use ::tower::Service;
use futures_util::TryStreamExt;

use crate::{
    CompactionOutput, GenerationOutput, ResponsesOutput, ResponsesServiceResponse,
    responses::{ContentItem, MessageRole},
    session::SessionId,
    tower::{ResponsePipelineStats, ResponsesAttemptKind},
};

use crate::{OpenAi, ResponseEvent};

use super::{ResponseError, response::estimate_cost};

#[derive(Clone)]
struct Scripted {
    calls: Arc<AtomicU32>,
}

impl Service<crate::ResponsesAttempt> for Scripted {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _context: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: crate::ResponsesAttempt) -> Self::Future {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        let item = crate::ResponseItem::message(
            MessageRole::Assistant,
            [ContentItem::OutputText {
                text: format!("answer-{call}").into(),
                annotations: None,
                logprobs: None,
            }],
        );
        Box::pin(async move {
            request
                .emit(ResponseEvent::OutputTextDelta(format!("answer-{call}")))
                .await;
            Ok(
                ResponsesServiceResponse::new(ResponsesOutput::Generation(GenerationOutput {
                    id: format!("resp-{call}"),
                    status: "completed".to_owned(),
                    end_turn: None,
                    final_message: Some(format!("answer-{call}")),
                    output_items: vec![item],
                    code_calls: Vec::new(),
                    usage: Some(crate::Usage {
                        input_tokens: 12,
                        output_tokens: 5,
                        total_tokens: 17,
                        ..crate::Usage::default()
                    }),
                    time_to_first_event_ns: 1,
                    time_to_first_output_ns: Some(1),
                    pipeline_stats: ResponsePipelineStats::default(),
                }))
                .with_connection_generation(1)
                .with_server_reasoning_included(true),
            )
        })
    }
}

#[tokio::test]
async fn response_stream_and_future_share_one_completed_operation() {
    let calls = Arc::new(AtomicU32::new(0));
    let factory_calls = Arc::clone(&calls);
    let openai = OpenAi::builder("test-key")
        .service(move || Scripted {
            calls: Arc::clone(&factory_calls),
        })
        .build()
        .unwrap();
    let mut session = openai
        .instructions("Answer only from supplied facts.")
        .build()
        .unwrap();
    let completed = {
        let mut turn = session.turn();
        let mut response = turn.create("The region is us-west-2.");

        let event = response.try_next().await.unwrap().unwrap();
        assert!(matches!(event, ResponseEvent::OutputTextDelta(delta) if delta == "answer-1"));
        let event = response.try_next().await.unwrap().unwrap();
        assert!(matches!(event, ResponseEvent::Completed { .. }));
        assert!(response.try_next().await.unwrap().is_none());
        response.await.unwrap()
    };

    assert_eq!(completed.output_text(), "answer-1");
    let estimated_cost = completed
        .estimated_cost()
        .expect("provider usage should produce an estimate");
    assert_eq!(estimated_cost.amount().decimal(), "0.00021");
    assert_eq!(
        completed.cost_status(),
        crate::CostStatus::EstimatedFromUsage
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(session.history_len(), 2);
    assert_eq!(session.active_context_tokens(), 17);
}

#[test]
fn missing_usage_never_becomes_a_zero_cost_estimate() {
    let (estimate, status) = estimate_cost(None, false);
    assert!(estimate.is_none());
    assert_eq!(status, crate::CostStatus::UsageNotReported);
}

#[derive(Debug)]
struct AttemptObservation {
    previous_response_id: Option<String>,
    full_replay: bool,
    input: Vec<serde_json::Value>,
}

#[derive(Clone)]
struct RecordingScripted {
    calls: Arc<AtomicU32>,
    observations: Arc<Mutex<Vec<AttemptObservation>>>,
}

impl Service<crate::ResponsesAttempt> for RecordingScripted {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _context: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: crate::ResponsesAttempt) -> Self::Future {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        self.observations.lock().unwrap().push(AttemptObservation {
            previous_response_id: request.previous_response_id().map(str::to_owned),
            full_replay: request.is_full_replay(),
            input: request
                .input_items()
                .map(|item| serde_json::to_value(item).unwrap())
                .collect(),
        });
        let item = crate::ResponseItem::message(
            MessageRole::Assistant,
            [ContentItem::OutputText {
                text: format!("answer-{call}").into(),
                annotations: None,
                logprobs: None,
            }],
        );
        std::future::ready(Ok(ResponsesServiceResponse::new(
            ResponsesOutput::Generation(GenerationOutput {
                id: format!("resp-{call}"),
                status: "completed".to_owned(),
                end_turn: Some(call == 2),
                final_message: Some(format!("answer-{call}")),
                output_items: vec![item],
                code_calls: Vec::new(),
                usage: None,
                time_to_first_event_ns: 1,
                time_to_first_output_ns: Some(1),
                pipeline_stats: ResponsePipelineStats::default(),
            }),
        )))
    }
}

#[tokio::test]
async fn sequential_creates_send_only_the_new_delta_after_completion() {
    let calls = Arc::new(AtomicU32::new(0));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = Arc::clone(&calls);
    let factory_observations = Arc::clone(&observations);
    let openai = OpenAi::builder("test-key")
        .service(move || RecordingScripted {
            calls: Arc::clone(&factory_calls),
            observations: Arc::clone(&factory_observations),
        })
        .build()
        .unwrap();
    let mut session = openai
        .instructions("Remember deployment facts between calls.")
        .build()
        .unwrap();

    {
        let mut turn = session.turn();
        assert_eq!(
            turn.create("The region is us-west-2.")
                .await
                .unwrap()
                .output_text(),
            "answer-1"
        );
        assert_eq!(
            turn.create("What region did I give you?")
                .await
                .unwrap()
                .output_text(),
            "answer-2"
        );
    }

    let observations = observations.lock().unwrap();
    assert_eq!(observations.len(), 2);
    assert!(observations[0].full_replay);
    assert_eq!(observations[0].previous_response_id, None);
    assert_eq!(observations[0].input.len(), 3);
    assert!(!observations[1].full_replay);
    assert_eq!(
        observations[1].previous_response_id.as_deref(),
        Some("resp-1")
    );
    assert_eq!(observations[1].input.len(), 1);
    assert_eq!(observations[1].input[0]["role"], "user");
    assert_eq!(session.history_len(), 4);
}

#[derive(Clone)]
struct CompactingScripted {
    calls: Arc<AtomicU32>,
    observations: Arc<Mutex<Vec<AttemptObservation>>>,
}

impl Service<crate::ResponsesAttempt> for CompactingScripted {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _context: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: crate::ResponsesAttempt) -> Self::Future {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        self.observations.lock().unwrap().push(AttemptObservation {
            previous_response_id: request.previous_response_id().map(str::to_owned),
            full_replay: request.is_full_replay(),
            input: request
                .input_items()
                .map(|item| serde_json::to_value(item).unwrap())
                .collect(),
        });
        let output = if matches!(request.kind(), ResponsesAttemptKind::Compaction) {
            ResponsesOutput::Compaction(CompactionOutput {
                id: format!("resp-{call}"),
                status: "completed".to_owned(),
                item: crate::ResponseItem::Compaction {
                    id: None,
                    encrypted_content: "encrypted-summary".into(),
                    created_by: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                usage: None,
                time_to_first_event_ns: 1,
                time_to_first_output_ns: Some(1),
                pipeline_stats: ResponsePipelineStats::default(),
            })
        } else {
            let item = crate::ResponseItem::message(
                MessageRole::Assistant,
                [ContentItem::OutputText {
                    text: format!("answer-{call}").into(),
                    annotations: None,
                    logprobs: None,
                }],
            );
            ResponsesOutput::Generation(GenerationOutput {
                id: format!("resp-{call}"),
                status: "completed".to_owned(),
                end_turn: None,
                final_message: Some(format!("answer-{call}")),
                output_items: vec![item],
                code_calls: Vec::new(),
                usage: None,
                time_to_first_event_ns: 1,
                time_to_first_output_ns: Some(1),
                pipeline_stats: ResponsePipelineStats::default(),
            })
        };
        std::future::ready(Ok(ResponsesServiceResponse::new(output)))
    }
}

#[tokio::test]
async fn compaction_atomically_replaces_history_and_forces_one_full_replay() {
    let calls = Arc::new(AtomicU32::new(0));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = Arc::clone(&calls);
    let factory_observations = Arc::clone(&observations);
    let openai = OpenAi::builder("test-key")
        .service(move || CompactingScripted {
            calls: Arc::clone(&factory_calls),
            observations: Arc::clone(&factory_observations),
        })
        .build()
        .unwrap();
    let mut session = openai
        .instructions("Retain user facts across explicit compaction.")
        .build()
        .unwrap();

    {
        let mut turn = session.turn();
        turn.create("The deployment region is us-west-2.")
            .await
            .unwrap();
        turn.compact().await.unwrap();
    }
    assert_eq!(session.history_len(), 2);
    assert!(session.history().any(crate::ResponseItem::is_user_message));
    assert!(
        session
            .history()
            .any(|item| matches!(item, crate::ResponseItem::Compaction { .. }))
    );

    session
        .turn()
        .create("Recall the deployment region.")
        .await
        .unwrap();

    let observations = observations.lock().unwrap();
    assert_eq!(observations.len(), 3);
    assert_eq!(
        observations[1].previous_response_id.as_deref(),
        Some("resp-1")
    );
    assert!(observations[2].full_replay);
    assert_eq!(observations[2].previous_response_id, None);
    assert_eq!(observations[2].input.len(), 5);
}

#[derive(Clone)]
struct FailingScripted;

impl Service<crate::ResponsesAttempt> for FailingScripted {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _context: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: crate::ResponsesAttempt) -> Self::Future {
        Box::pin(async move {
            request
                .emit(ResponseEvent::OutputTextDelta("uncommitted".to_owned()))
                .await;
            Err(ResponseError::service(std::io::Error::other(
                "scripted failure",
            )))
        })
    }
}

#[tokio::test]
async fn failed_partial_response_never_commits_input_or_output() {
    let openai = OpenAi::builder("test-key")
        .service(|| FailingScripted)
        .build()
        .unwrap();
    let mut session = openai
        .instructions("Commit only complete Responses operations.")
        .build()
        .unwrap();
    {
        let mut turn = session.turn();
        let mut response = turn.create("This input must remain uncommitted.");

        assert!(matches!(
            response.try_next().await.unwrap(),
            Some(ResponseEvent::OutputTextDelta(delta)) if delta == "uncommitted"
        ));
        let error = response.try_next().await.unwrap_err();
        assert_eq!(error.to_string(), "scripted failure");
        assert!(response.await.is_err());
    }

    assert_eq!(session.history_len(), 0);
}

#[test]
fn boxed_tower_errors_preserve_context_window_classification() {
    let service_error =
        crate::ResponsesServiceError::from(crate::ResponsesError::ContextWindowExceeded {
            event: r#"{"error":{"code":"context_length_exceeded"}}"#.to_owned(),
        });
    let error = ResponseError::from(Box::new(service_error) as ::tower::BoxError);

    assert!(error.is_context_window_exceeded());
    assert!(error.source().is_some());
}

#[tokio::test]
async fn dropping_an_unpolled_response_performs_no_work() {
    let calls = Arc::new(AtomicU32::new(0));
    let factory_calls = Arc::clone(&calls);
    let openai = OpenAi::builder("test-key")
        .service(move || Scripted {
            calls: Arc::clone(&factory_calls),
        })
        .build()
        .unwrap();
    let mut session = openai
        .instructions("Do not run abandoned operations.")
        .build()
        .unwrap();
    {
        let mut turn = session.turn();
        drop(turn.create("abandoned"));
    }

    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(session.history_len(), 0);
}

#[test]
fn session_ids_are_serializable_uuid_v7_values() {
    let id = SessionId::new();
    assert_eq!(id.as_uuid().get_version_num(), 7);

    let encoded = serde_json::to_string(&id).unwrap();
    assert_eq!(serde_json::from_str::<SessionId>(&encoded).unwrap(), id);
    assert!(
        "550e8400-e29b-41d4-a716-446655440000"
            .parse::<SessionId>()
            .is_err()
    );
}
