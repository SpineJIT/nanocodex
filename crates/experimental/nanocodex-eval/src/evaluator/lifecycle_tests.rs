use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use nanocodex_agent::{Nanocodex, NanocodexBuilder, OpenAi, Tools};
use nanocodex_oai_api::{
    pricing::CostStatus,
    responses::{InputTokenDetails, OutputTokenDetails, Usage},
};
use nanocodex_tools::{ToolContext, ToolDefinition, ToolOutput, runtime::DynamicToolProvider};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use super::{
    AgentEvent, AgentEventKind, AgentObservation, AttemptAgent, AttemptVerification,
    AttemptVerifier, AttemptVerifierCleanupFuture, AttemptVerifierFuture, EvalAttempt, Evaluator,
};
use crate::{
    AgentStatus, BillingCompleteness, CleanupPhase, CleanupStatus, EvalAttemptOutcome,
    EvalEventKind, EvalExceptionKind, EvalOutcome, EvalStatus, Task, VerifierResult,
    harbor::Harbor,
};

struct AttemptResourceProvider {
    live_resources: Arc<AtomicUsize>,
}

struct PackageMutatingProvider {
    mutation: PathBuf,
}

impl Drop for AttemptResourceProvider {
    fn drop(&mut self) {
        self.live_resources.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for PackageMutatingProvider {
    fn drop(&mut self) {
        fs::write(&self.mutation, "changed after agent execution\n").unwrap();
    }
}

#[async_trait]
impl DynamicToolProvider for AttemptResourceProvider {
    fn start(&self) {}

    fn direct_tools(&self) -> Vec<Arc<dyn nanocodex_tools::Tool>> {
        Vec::new()
    }

    fn available_definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    async fn execute(
        &self,
        _name: &str,
        _input: Value,
        _context: ToolContext<'_>,
    ) -> Option<ToolOutput> {
        None
    }
}

#[async_trait]
impl DynamicToolProvider for PackageMutatingProvider {
    fn start(&self) {}

    fn direct_tools(&self) -> Vec<Arc<dyn nanocodex_tools::Tool>> {
        Vec::new()
    }

    fn available_definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    async fn execute(
        &self,
        _name: &str,
        _input: Value,
        _context: ToolContext<'_>,
    ) -> Option<ToolOutput> {
        None
    }
}

struct ResourceProbeVerifier {
    live_resources: Arc<AtomicUsize>,
}

struct ResourceCheckedVerifier<V> {
    inner: V,
    live_resources: Arc<AtomicUsize>,
}

struct ShutdownProbeVerifier {
    shutdowns: Arc<AtomicUsize>,
}

struct FailingCleanupVerifier;

struct StaticVerifier {
    reward: f64,
}

struct TimeoutRun {
    outcome: EvalAttemptOutcome,
    trial: Value,
    trajectory: Value,
    job: Value,
    aggregate: crate::AggregateDataset,
    terminal_events: usize,
    live_resources: usize,
}

impl AttemptVerifier for ShutdownProbeVerifier {
    fn verify<'a>(
        &'a mut self,
        _task: &'a Task,
        _attempt: EvalAttempt<'a>,
    ) -> AttemptVerifierFuture<'a> {
        Box::pin(async {
            panic!("shutdown probe verifier must not execute after an earlier failure")
        })
    }

    fn shutdown(&mut self) -> AttemptVerifierCleanupFuture<'_> {
        Box::pin(async move {
            self.shutdowns.fetch_add(1, Ordering::AcqRel);
            CleanupPhase::completed(chrono::Utc::now())
        })
    }
}

impl AttemptVerifier for FailingCleanupVerifier {
    fn verify<'a>(
        &'a mut self,
        _task: &'a Task,
        attempt: EvalAttempt<'a>,
    ) -> AttemptVerifierFuture<'a> {
        Box::pin(async move {
            let cleanup_error = std::io::Error::other("deterministic verifier cleanup failure");
            if let Err(error) = fs::write(attempt.directory().join("verifier/test-stdout.txt"), [])
            {
                return Err(super::AttemptVerificationFailure::new(
                    error,
                    CleanupPhase::not_required(),
                ));
            }
            let primary = std::io::Error::other("deterministic verifier primary failure");
            let occurred_at = chrono::Utc::now();
            let cleanup_started = chrono::Utc::now();
            Err(super::AttemptVerificationFailure::observed_at(
                primary,
                occurred_at,
                CleanupPhase::failed(cleanup_started, &cleanup_error),
            ))
        })
    }
}

impl AttemptVerifier for StaticVerifier {
    fn verify<'a>(
        &'a mut self,
        _task: &'a Task,
        attempt: EvalAttempt<'a>,
    ) -> AttemptVerifierFuture<'a> {
        let reward = self.reward;
        Box::pin(async move {
            fs::write(attempt.directory().join("verifier/test-stdout.txt"), []).map_err(
                |error| super::AttemptVerificationFailure::new(error, CleanupPhase::not_required()),
            )?;
            Ok(AttemptVerification {
                result: VerifierResult {
                    exit_code: i32::from(reward <= 0.0),
                    rewards: BTreeMap::from([("reward".to_owned(), reward)]),
                },
                stdout: String::new(),
                stderr: String::new(),
                cleanup: CleanupPhase::not_required(),
            })
        })
    }
}

impl AttemptVerifier for ResourceProbeVerifier {
    fn verify<'a>(
        &'a mut self,
        _task: &'a Task,
        _attempt: EvalAttempt<'a>,
    ) -> AttemptVerifierFuture<'a> {
        assert_eq!(
            self.live_resources.load(Ordering::Acquire),
            0,
            "attempt-owned agent resources must be joined before verification starts"
        );
        Box::pin(async {
            let cleanup_started = chrono::Utc::now();
            let cleanup_error = std::io::Error::other("deterministic verifier cleanup failure");
            Ok(AttemptVerification {
                result: VerifierResult {
                    exit_code: 0,
                    rewards: BTreeMap::from([("reward".to_owned(), 1.0)]),
                },
                stdout: String::new(),
                stderr: String::new(),
                cleanup: CleanupPhase::failed(cleanup_started, &cleanup_error),
            })
        })
    }
}

impl<V> AttemptVerifier for ResourceCheckedVerifier<V>
where
    V: AttemptVerifier,
{
    fn verify<'a>(
        &'a mut self,
        task: &'a Task,
        attempt: EvalAttempt<'a>,
    ) -> AttemptVerifierFuture<'a> {
        assert_eq!(
            self.live_resources.load(Ordering::Acquire),
            0,
            "timed-out agent resources must be joined before verification starts"
        );
        self.inner.verify(task, attempt)
    }

    fn shutdown(&mut self) -> AttemptVerifierCleanupFuture<'_> {
        self.inner.shutdown()
    }
}

#[test]
fn verifier_failure_default_timestamp_does_not_follow_completed_cleanup() {
    let cleanup_started = chrono::Utc::now();
    let failure = super::AttemptVerificationFailure::new(
        std::io::Error::other("deterministic verifier failure"),
        CleanupPhase::completed(cleanup_started),
    );
    let (_, occurred_at, cleanup) = failure.into_parts();

    assert_eq!(occurred_at, cleanup_started);
    assert!(cleanup.timing.is_some_and(|timing| {
        occurred_at <= timing.started_at && timing.started_at <= timing.finished_at
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn agent_resources_are_joined_before_attempt_verifier() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let warmup = socket.next().await.unwrap().unwrap();
        assert!(warmup.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": { "id": "resp-warmup", "usage": null }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let generation = socket.next().await.unwrap().unwrap();
        assert!(generation.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp-generation",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": "done" }]
                        }],
                        "usage": {
                            "input_tokens": 1,
                            "input_tokens_details": { "cached_tokens": 0 },
                            "output_tokens": 1,
                            "output_tokens_details": { "reasoning_tokens": 0 },
                            "total_tokens": 2
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        while socket.next().await.is_some() {}
    });
    let live_resources = Arc::new(AtomicUsize::new(0));
    let openai = OpenAi::builder("test")
        .websocket_url(endpoint)
        .build()
        .unwrap();
    let tool_resources = Arc::clone(&live_resources);
    let nanocodex = Nanocodex::builder(openai).tools_factory(move |_agent| {
        tool_resources.fetch_add(1, Ordering::AcqRel);
        Tools::builder()
            .without_defaults()
            .provider(AttemptResourceProvider {
                live_resources: Arc::clone(&tool_resources),
            })
            .build()
    });
    let output = tempdir().unwrap();
    let verifier_resources = Arc::clone(&live_resources);
    let evaluator = Evaluator::new_builder(nanocodex)
        .output_directory(output.path())
        .attempt_agent(move |_attempt, builder| {
            Ok::<_, Infallible>(AttemptAgent::new(builder).verifier(ResourceProbeVerifier {
                live_resources: Arc::clone(&verifier_resources),
            }))
        })
        .build()
        .unwrap();
    let task = Task::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
    )
    .unwrap();

    let outcome = evaluator.task(task).await.unwrap();
    let result = outcome
        .scored()
        .expect("the successful verifier must return a scored outcome");

    assert_eq!(result.status, EvalStatus::Passed);
    assert_eq!(result.outcome, EvalOutcome::Passed);
    assert!(result.exception.is_none());
    assert_eq!(result.cleanup.agent.status, CleanupStatus::Completed);
    assert_eq!(result.cleanup.verifier.status, CleanupStatus::Failed);
    assert_eq!(live_resources.load(Ordering::Acquire), 0);
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn verifier_primary_and_cleanup_failures_are_both_retained() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let warmup = socket.next().await.unwrap().unwrap();
        assert!(warmup.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": { "id": "resp-warmup", "usage": null }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let generation = socket.next().await.unwrap().unwrap();
        assert!(generation.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp-generation",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": "done" }]
                        }],
                        "usage": {
                            "input_tokens": 1,
                            "input_tokens_details": { "cached_tokens": 0 },
                            "output_tokens": 1,
                            "output_tokens_details": { "reasoning_tokens": 0 },
                            "total_tokens": 2
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        while socket.next().await.is_some() {}
    });
    let openai = OpenAi::builder("test")
        .websocket_url(endpoint)
        .build()
        .unwrap();
    let output = tempdir().unwrap();
    let evaluator = Evaluator::new_builder(Nanocodex::builder(openai))
        .output_directory(output.path())
        .attempt_agent(|_attempt, builder| {
            Ok::<_, Infallible>(AttemptAgent::new(builder).verifier(FailingCleanupVerifier))
        })
        .build()
        .unwrap();
    let task = Task::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
    )
    .unwrap();

    let outcome = evaluator.task(task).await.unwrap();
    let failure = outcome
        .unscored()
        .expect("verifier execution failure must be unscored");

    assert_eq!(failure.exception.kind, crate::EvalExceptionKind::Verifier);
    assert!(
        failure
            .exception
            .message
            .contains("deterministic verifier primary failure")
    );
    assert!(
        failure
            .exception
            .traceback
            .contains("deterministic verifier primary failure")
    );
    assert_eq!(failure.cleanup.verifier.status, CleanupStatus::Failed);
    assert!(
        failure
            .cleanup
            .verifier
            .timing
            .as_ref()
            .is_some_and(|timing| { failure.exception.occurred_at <= timing.started_at })
    );
    assert!(
        failure
            .cleanup
            .verifier
            .diagnostic
            .as_ref()
            .is_some_and(|diagnostic| diagnostic
                .message
                .contains("deterministic verifier cleanup failure"))
    );
    assert!(failure.timing.verifier.is_some());
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn verifier_is_joined_after_post_agent_package_validation_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let warmup = socket.next().await.unwrap().unwrap();
        assert!(warmup.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": { "id": "resp-warmup", "usage": null }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let generation = socket.next().await.unwrap().unwrap();
        assert!(generation.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp-generation",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": "done" }]
                        }],
                        "usage": {
                            "input_tokens": 1,
                            "input_tokens_details": { "cached_tokens": 0 },
                            "output_tokens": 1,
                            "output_tokens_details": { "reasoning_tokens": 0 },
                            "total_tokens": 2
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        while socket.next().await.is_some() {}
    });
    let (task_directory, task) = task_with_agent_timeout(5.0);
    let mutation = task_directory.path().join("environment/README.md");
    let openai = OpenAi::builder("test")
        .websocket_url(endpoint)
        .build()
        .unwrap();
    let nanocodex = Nanocodex::builder(openai).tools_factory(move |_agent| {
        Tools::builder()
            .without_defaults()
            .provider(PackageMutatingProvider {
                mutation: mutation.clone(),
            })
            .build()
    });
    let output = tempdir().unwrap();
    let verifier_shutdowns = Arc::new(AtomicUsize::new(0));
    let verifier_shutdowns_for_attempt = Arc::clone(&verifier_shutdowns);
    let evaluator = Evaluator::new_builder(nanocodex)
        .output_directory(output.path())
        .attempt_agent(move |_attempt, builder| {
            Ok::<_, Infallible>(AttemptAgent::new(builder).verifier(ShutdownProbeVerifier {
                shutdowns: Arc::clone(&verifier_shutdowns_for_attempt),
            }))
        })
        .build()
        .unwrap();

    let outcome = evaluator.task(task).await.unwrap();
    let failure = outcome
        .unscored()
        .expect("post-agent package mutation must be returned as unscored");

    assert_eq!(
        failure.exception.kind,
        crate::EvalExceptionKind::Environment
    );
    assert_eq!(failure.cleanup.agent.status, CleanupStatus::Completed);
    assert_eq!(failure.cleanup.verifier.status, CleanupStatus::Completed);
    assert_eq!(verifier_shutdowns.load(Ordering::Acquire), 1);
    assert!(
        failure
            .cleanup
            .verifier
            .timing
            .as_ref()
            .is_some_and(|timing| { failure.exception.occurred_at <= timing.started_at })
    );
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn safety_refusal_runs_verifier_and_retains_independent_score_axes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let warmup = socket.next().await.unwrap().unwrap();
        assert!(warmup.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": { "id": "resp-warmup", "usage": null }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let generation = socket.next().await.unwrap().unwrap();
        assert!(generation.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": "resp-failed",
                        "status": "failed",
                        "error": {
                            "code": "cyber_policy",
                            "message": "deterministic safety refusal"
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        while socket.next().await.is_some() {}
    });
    let live_resources = Arc::new(AtomicUsize::new(0));
    let openai = OpenAi::builder("test")
        .websocket_url(endpoint)
        .build()
        .unwrap();
    let tool_resources = Arc::clone(&live_resources);
    let nanocodex = Nanocodex::builder(openai).tools_factory(move |_agent| {
        tool_resources.fetch_add(1, Ordering::AcqRel);
        Tools::builder()
            .without_defaults()
            .provider(AttemptResourceProvider {
                live_resources: Arc::clone(&tool_resources),
            })
            .build()
    });
    let output = tempdir().unwrap();
    let verifier_resources = Arc::clone(&live_resources);
    let evaluator = Evaluator::new_builder(nanocodex)
        .output_directory(output.path())
        .attempt_agent(move |_attempt, builder| {
            Ok::<_, Infallible>(
                AttemptAgent::new(builder).verifier(ResourceCheckedVerifier {
                    inner: StaticVerifier { reward: 1.0 },
                    live_resources: Arc::clone(&verifier_resources),
                }),
            )
        })
        .build()
        .unwrap();
    let task = Task::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
    )
    .unwrap();
    let run = evaluator.task(task);
    let recorder = crate::harbor::Harbor::new(&evaluator)
        .unwrap()
        .record(run.events().subscribe())
        .unwrap();

    let outcome = run
        .await
        .expect("an accepted provider failure must return a terminal outcome");

    assert_eq!(live_resources.load(Ordering::Acquire), 0);
    let result = outcome
        .scored()
        .expect("a healthy verifier must score a provider safety refusal");
    assert_eq!(result.status, EvalStatus::Passed);
    assert_eq!(result.outcome, EvalOutcome::SafetyRefusal);
    assert_eq!(
        result.exception.as_ref().map(|exception| exception.kind),
        Some(crate::EvalExceptionKind::AgentSafetyRefusal)
    );
    assert_eq!(result.verifier.rewards["reward"], 1.0);
    assert_eq!(result.cleanup.agent.status, CleanupStatus::Completed);
    assert_eq!(result.cleanup.verifier.status, CleanupStatus::NotRequired);
    assert!(result.cleanup.agent.timing.is_some());
    assert!(result.timing.queue_wait.finished_at >= result.timing.queue_wait.started_at);
    let agent = result
        .agent
        .as_ref()
        .expect("terminal run metrics must survive the provider failure");
    assert_eq!(agent.metadata.status, AgentStatus::Failed);
    assert_eq!(agent.billing_completeness, BillingCompleteness::Unknown);
    let job = recorder.finish(vec![outcome]).await.unwrap();
    let aggregate = job.aggregate_dataset().unwrap();
    let fact = &aggregate.attempts[0];
    assert!(fact.scored);
    assert!(fact.passed);
    assert!(fact.errored);
    assert!(fact.refused);
    assert_eq!(
        fact.exception_kind,
        Some(crate::EvalExceptionKind::AgentSafetyRefusal)
    );
    assert_eq!(aggregate.configurations[0].success.successes, 1);
    assert_eq!(aggregate.configurations[0].errored_attempts, 1);
    assert_eq!(aggregate.configurations[0].refused_attempts, 1);
    let job_result: Value =
        serde_json::from_slice(&fs::read(job.directory().join("result.json")).unwrap()).unwrap();
    assert_eq!(job_result["stats"]["n_completed_trials"], 1);
    assert_eq!(job_result["stats"]["n_errored_trials"], 1);
    let eval = job_result["stats"]["evals"]
        .as_object()
        .and_then(|evals| evals.values().next())
        .unwrap();
    assert_eq!(eval["n_trials"], 1);
    assert_eq!(eval["n_errors"], 1);
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn verifier_is_joined_after_attempt_readiness_failure() {
    let output = tempdir().unwrap();
    let verifier_shutdowns = Arc::new(AtomicUsize::new(0));
    let verifier_shutdowns_for_attempt = Arc::clone(&verifier_shutdowns);
    let nanocodex = Nanocodex::builder(OpenAi::new("test").unwrap());
    let evaluator = Evaluator::new_builder(nanocodex)
        .output_directory(output.path())
        .attempt_agent(move |_attempt, builder| {
            Ok::<_, Infallible>(
                AttemptAgent::new(builder)
                    .ready(async {
                        Err(std::io::Error::other(
                            "deterministic attempt readiness failure",
                        ))
                    })
                    .verifier(ShutdownProbeVerifier {
                        shutdowns: Arc::clone(&verifier_shutdowns_for_attempt),
                    }),
            )
        })
        .build()
        .unwrap();
    let task = Task::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
    )
    .unwrap();

    let outcome = evaluator.task(task).await.unwrap();
    let failure = outcome
        .unscored()
        .expect("readiness failure must be returned as unscored");

    assert_eq!(
        failure.exception.kind,
        crate::EvalExceptionKind::Environment
    );
    assert_eq!(failure.cleanup.agent.status, CleanupStatus::NotRequired);
    assert_eq!(failure.cleanup.verifier.status, CleanupStatus::Completed);
    assert_eq!(verifier_shutdowns.load(Ordering::Acquire), 1);
    assert!(
        failure
            .cleanup
            .verifier
            .timing
            .as_ref()
            .is_some_and(|timing| { failure.exception.occurred_at <= timing.started_at })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn verifier_is_joined_after_attempt_driver_preparation_failure() {
    let output = tempdir().unwrap();
    let verifier_shutdowns = Arc::new(AtomicUsize::new(0));
    let verifier_shutdowns_for_attempt = Arc::clone(&verifier_shutdowns);
    let nanocodex = Nanocodex::builder(OpenAi::new("test").unwrap());
    let evaluator = Evaluator::new_builder(nanocodex)
        .output_directory(output.path())
        .attempt_agent(move |_attempt, builder| {
            Ok::<_, Infallible>(
                AttemptAgent::preparing_nanocodex(async move {
                    drop(builder);
                    Err::<NanocodexBuilder, _>(std::io::Error::other(
                        "deterministic attempt preparation failure",
                    ))
                })
                .verifier(ShutdownProbeVerifier {
                    shutdowns: Arc::clone(&verifier_shutdowns_for_attempt),
                }),
            )
        })
        .build()
        .unwrap();
    let task = Task::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
    )
    .unwrap();

    let outcome = evaluator.task(task).await.unwrap();
    let failure = outcome
        .unscored()
        .expect("preparation failure must be returned as unscored");

    assert_eq!(
        failure.exception.kind,
        crate::EvalExceptionKind::Environment
    );
    assert_eq!(failure.cleanup.agent.status, CleanupStatus::NotRequired);
    assert_eq!(failure.cleanup.verifier.status, CleanupStatus::Completed);
    assert_eq!(verifier_shutdowns.load(Ordering::Acquire), 1);
    assert!(
        failure
            .cleanup
            .verifier
            .timing
            .as_ref()
            .is_some_and(|timing| { failure.exception.occurred_at <= timing.started_at })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn model_in_flight_timeout_can_retain_a_passing_verifier_score() {
    let run = run_timed_out_attempt(|| StaticVerifier { reward: 1.0 }).await;
    let result = run
        .outcome
        .scored()
        .expect("a completed verifier must make the timeout scored");

    assert_eq!(run.live_resources, 0);
    assert_eq!(run.terminal_events, 1);
    assert_eq!(result.status, EvalStatus::Passed);
    assert_eq!(result.outcome, EvalOutcome::AgentTimeout);
    assert_eq!(
        result.exception.as_ref().map(|exception| exception.kind),
        Some(EvalExceptionKind::AgentTimeout)
    );
    assert!(
        result
            .exception
            .as_ref()
            .is_some_and(|exception| exception.occurred_at <= result.timing.verifier.started_at)
    );
    let agent = result
        .agent
        .as_ref()
        .expect("cancellation must retain a partial terminal snapshot");
    assert_eq!(agent.metadata.status, AgentStatus::Cancelled);
    assert_eq!(agent.billing_completeness, BillingCompleteness::Unknown);
    assert_eq!(
        agent.metadata.runtime_completeness,
        crate::MeasurementCompleteness::ObservedLowerBound
    );
    assert_eq!(run.trial["scored"], true);
    assert_eq!(run.trial["outcome"], "agent_timeout");
    assert_eq!(
        run.trial["exception_info"]["exception_type"],
        "AgentTimeoutError"
    );
    assert_eq!(run.trial["verifier_result"]["rewards"]["reward"], 1.0);
    assert_eq!(run.trial["agent_result"]["billing_completeness"], "unknown");
    assert_eq!(
        run.trial["agent_result"]["metadata"]["runtime_completeness"],
        "observed_lower_bound"
    );
    assert_eq!(run.job["stats"]["n_completed_trials"], 1);
    assert_eq!(run.job["stats"]["n_errored_trials"], 1);
    assert_eq!(run.job["stats"]["n_billing_missing_trials"], 1);
    let eval = run.job["stats"]["evals"]
        .as_object()
        .and_then(|evals| evals.values().next())
        .expect("the scored timeout must contribute one eval aggregate");
    assert_eq!(eval["n_trials"], 1);
    assert_eq!(eval["n_errors"], 1);
    assert_eq!(
        run.aggregate.configurations[0].tokens.total_tokens.samples,
        0
    );
    assert_eq!(
        run.aggregate.configurations[0]
            .observed_tokens_lower_bound
            .total_tokens
            .samples,
        0
    );
    assert_eq!(run.aggregate.configurations[0].billing_missing_attempts, 1);
    assert!(run.aggregate.attempts[0].usage.is_none());
    assert_eq!(
        run.aggregate.attempts[0]
            .runtime
            .as_ref()
            .map(|runtime| runtime.completeness),
        Some(crate::MeasurementCompleteness::ObservedLowerBound)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn model_in_flight_timeout_can_retain_a_failing_verifier_score() {
    let run = run_timed_out_attempt(|| StaticVerifier { reward: 0.0 }).await;
    let result = run
        .outcome
        .scored()
        .expect("a completed verifier must make the timeout scored");

    assert_eq!(run.terminal_events, 1);
    assert_eq!(result.status, EvalStatus::Failed);
    assert_eq!(result.outcome, EvalOutcome::AgentTimeout);
    assert_eq!(
        result.exception.as_ref().map(|exception| exception.kind),
        Some(EvalExceptionKind::AgentTimeout)
    );
    assert_eq!(run.trial["verifier_result"]["rewards"]["reward"], 0.0);
}

#[tokio::test(flavor = "current_thread")]
async fn verifier_failure_after_timeout_preserves_the_agent_error() {
    let run = run_timed_out_attempt(|| FailingCleanupVerifier).await;
    let failure = run
        .outcome
        .unscored()
        .expect("a verifier execution failure cannot retain a score");

    assert_eq!(run.terminal_events, 1);
    assert_eq!(failure.exception.kind, EvalExceptionKind::AgentTimeout);
    assert_eq!(failure.exception.outcome, EvalOutcome::AgentTimeout);
    assert!(failure.timing.verifier.as_ref().is_some_and(|timing| {
        failure.exception.occurred_at <= timing.started_at
            && timing.finished_at <= failure.finished_at
    }));
    assert_eq!(failure.cleanup.verifier.status, CleanupStatus::Failed);
    assert!(failure.verifier.is_none());
    assert_eq!(
        run.trial["exception_info"]["exception_type"],
        "AgentTimeoutError"
    );
    assert!(run.trial["verifier_result"].is_null());
    assert_eq!(
        run.trajectory["final_metrics"]["extra"]["billing_completeness"],
        "unknown"
    );
    assert!(
        run.trajectory["final_metrics"]["extra"]["response_attempts"]
            .as_u64()
            .is_some_and(|attempts| attempts > 0)
    );
    assert_eq!(run.job["stats"]["n_completed_trials"], 1);
    assert_eq!(run.job["stats"]["n_errored_trials"], 1);
}

#[tokio::test(flavor = "current_thread")]
async fn post_setup_agent_failure_can_retain_a_verifier_score() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": { "id": "resp-warmup", "usage": null }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(Message::Text(
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": "resp-failed",
                        "status": "failed",
                        "error": {
                            "code": "invalid_request_error",
                            "message": "deterministic post-setup agent failure"
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        while socket.next().await.is_some() {}
    });
    let openai = OpenAi::builder("test")
        .websocket_url(endpoint)
        .build()
        .unwrap();
    let output = tempdir().unwrap();
    let evaluator = Evaluator::new_builder(Nanocodex::builder(openai))
        .output_directory(output.path())
        .attempt_agent(|_attempt, builder| {
            Ok::<_, Infallible>(AttemptAgent::new(builder).verifier(StaticVerifier { reward: 1.0 }))
        })
        .build()
        .unwrap();
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();

    let outcome = evaluator.task(task).await.unwrap();
    let result = outcome
        .scored()
        .expect("post-setup agent failure must retain completed verification");
    assert_eq!(result.status, EvalStatus::Passed);
    assert_eq!(result.outcome, EvalOutcome::InfrastructureError);
    assert_eq!(
        result.exception.as_ref().map(|exception| exception.kind),
        Some(EvalExceptionKind::Agent)
    );
    assert_eq!(
        result.agent.as_ref().map(|agent| agent.metadata.status),
        Some(AgentStatus::Failed)
    );
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_terminal_metrics_retain_usage_and_verifier_score() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let warmup = socket.next().await.unwrap().unwrap();
        assert!(warmup.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": { "id": "resp-warmup", "usage": null }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let generation = socket.next().await.unwrap().unwrap();
        assert!(generation.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp-generation",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": "done" }]
                        }],
                        "usage": {
                            "input_tokens": 1,
                            "input_tokens_details": { "cached_tokens": 0 },
                            "output_tokens": 1,
                            "output_tokens_details": { "reasoning_tokens": 0 },
                            "total_tokens": 2
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        while socket.next().await.is_some() {}
    });
    let openai = OpenAi::builder("test")
        .websocket_url(endpoint)
        .build()
        .unwrap();
    let output = tempdir().unwrap();
    let evaluator = Evaluator::new_builder(Nanocodex::builder(openai))
        .with_malformed_terminal_metrics()
        .output_directory(output.path())
        .attempt_agent(|_attempt, builder| {
            Ok::<_, Infallible>(AttemptAgent::new(builder).verifier(StaticVerifier { reward: 1.0 }))
        })
        .build()
        .unwrap();
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let run = evaluator.task(task);
    let recorder = crate::harbor::Harbor::new(&evaluator)
        .unwrap()
        .record(run.events().subscribe())
        .unwrap();

    let outcome = run.await.unwrap();
    let result = outcome
        .scored()
        .expect("malformed terminal metrics must not prevent verification");

    assert_eq!(result.status, EvalStatus::Passed);
    assert_eq!(result.outcome, EvalOutcome::InfrastructureError);
    assert_eq!(
        result.exception.as_ref().map(|exception| exception.kind),
        Some(EvalExceptionKind::Agent)
    );
    assert!(
        result
            .exception
            .as_ref()
            .is_some_and(|exception| exception.message.contains("terminal metrics"))
    );
    let agent = result
        .agent
        .as_ref()
        .expect("completed operation metrics must remain available");
    assert_eq!(agent.metadata.status, AgentStatus::Completed);
    assert_eq!(agent.billing_completeness, BillingCompleteness::Unknown);
    assert_eq!(agent.cost_usd, None);
    assert_eq!(
        agent.metadata.cost_status,
        CostStatus::UsageNotReported.as_str()
    );
    assert!(agent.metadata.estimated_cost.is_none());
    assert_eq!(result.verifier.rewards.get("reward").copied(), Some(1.0));
    let attempt_directory = result.artifacts.directory.clone();
    let job = recorder.finish(vec![outcome]).await.unwrap();
    let aggregate = job.aggregate_dataset().unwrap();
    assert!(aggregate.attempts[0].estimated_cost.is_none());
    assert_eq!(
        aggregate.configurations[0]
            .observed_cost_components_lower_bound_usd
            .total_usd
            .samples,
        0
    );
    assert_eq!(
        aggregate.configurations[0]
            .cost_components_usd
            .total_usd
            .samples,
        0
    );
    assert_eq!(
        aggregate.configurations[0]
            .observed_tokens_lower_bound
            .total_tokens
            .samples,
        1
    );
    assert_eq!(aggregate.configurations[0].tokens.total_tokens.samples, 0);
    let events = fs::read_to_string(attempt_directory.join("agent/events.jsonl"))
        .expect("retained agent events");
    let terminal_event = events
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| matches!(event["type"].as_str(), Some("run.completed" | "run.failed")))
        .expect("retained terminal event");
    assert_eq!(terminal_event["type"], "run.completed");
    let retained_result: Value = serde_json::from_slice(
        &fs::read(attempt_directory.join("result.json")).expect("retained Harbor result"),
    )
    .expect("valid retained Harbor result");
    assert_eq!(
        retained_result["agent_result"]["metadata"]["status"],
        "completed"
    );
    let trajectory: Value = serde_json::from_slice(
        &fs::read(attempt_directory.join("agent/trajectory.json"))
            .expect("retained ATIF trajectory"),
    )
    .expect("valid retained ATIF trajectory");
    let terminal_step = trajectory["steps"]
        .as_array()
        .and_then(|steps| steps.iter().rev().find(|step| step["source"] == "agent"))
        .expect("terminal ATIF agent step");
    assert_eq!(
        terminal_step["extra"]["terminal_event_type"],
        "run.completed"
    );
    assert_eq!(
        terminal_step["extra"]["terminal_payload"]["status"],
        "completed"
    );
    server.await.unwrap();
}

#[test]
fn malformed_terminal_metrics_are_a_verifier_usable_agent_failure() {
    let error = super::EvalError::AgentTerminal(
        serde_json::from_str::<Value>("{").expect_err("fixture must be malformed"),
    );

    assert_eq!(super::failure_kind(&error), EvalExceptionKind::Agent);
    assert!(super::verifier_workspace_usable_after_agent_error(&error));
}

#[test]
fn completed_model_call_leaves_idle_tool_timeout_billing_complete() {
    let mut observation = AgentObservation::default();
    observation.observe_lifecycle(AgentEventKind::ModelCallStarted);
    observation.observe_lifecycle(AgentEventKind::ModelCallCompleted);

    assert_eq!(observation.billable_in_flight, 0);
    assert_eq!(
        observation.billing_completeness(),
        BillingCompleteness::Complete
    );
}

#[test]
fn shared_prefix_warmup_is_not_a_billable_completed_response() {
    let mut observation = AgentObservation::default();
    observation
        .observe(&agent_event(
            1,
            AgentEventKind::ModelWarmupStarted,
            json!({"model": "gpt-5.6-sol", "prompt_cache_key": "shared"}),
        ))
        .unwrap();
    observation
        .observe(&agent_event(
            2,
            AgentEventKind::ModelWarmupCompleted,
            json!({
                "source": "shared_prefix",
                "attempt": null,
                "connection_generation": null,
                "duration_ns": 10,
                "usage": null,
            }),
        ))
        .unwrap();

    assert_eq!(
        observation.billing_completeness(),
        BillingCompleteness::Complete
    );
    assert!(observation.lower_bound_result(None).is_none());
}

#[test]
fn completed_operations_form_an_exact_timeout_lower_bound() {
    let warmup_usage = provider_usage(100, 40, 10, 20, 5);
    let generation_usage = provider_usage(200, 100, 20, 30, 8);
    let compaction_usage = provider_usage(50, 10, 5, 6, 0);
    let events = [
        agent_event(
            1,
            AgentEventKind::RunStarted,
            json!({
                "mode": "openai_model",
                "model": "gpt-5.6-sol",
                "reasoning_mode": "summary",
                "effort": "high",
                "transport": "websocket",
                "orchestration": "agent",
                "websocket_url": "wss://example.invalid",
                "workspace": null,
                "instruction_bytes": 4,
            }),
        ),
        agent_event(
            2,
            AgentEventKind::ModelWarmupStarted,
            json!({"model": "gpt-5.6-sol", "prompt_cache_key": "cache"}),
        ),
        agent_event(
            3,
            AgentEventKind::ModelWarmupCompleted,
            json!({
                "source": "response",
                "attempt": 1,
                "connection_generation": 1,
                "duration_ns": 11,
                "usage": warmup_usage,
            }),
        ),
        agent_event(
            4,
            AgentEventKind::ModelCallStarted,
            json!({
                "call_index": 1,
                "model": "gpt-5.6-sol",
                "reasoning_mode": "summary",
                "effort": "high",
            }),
        ),
        agent_event(
            5,
            AgentEventKind::ModelCallCompleted,
            json!({
                "call_index": 1,
                "model": "gpt-5.6-sol",
                "attempt": 2,
                "connection_generation": 1,
                "status": "completed",
                "duration_ns": 22,
                "time_to_first_event_ns": 2,
                "time_to_first_output_ns": 3,
                "tool_calls": 2,
                "usage": generation_usage,
            }),
        ),
        agent_event(
            6,
            AgentEventKind::ModelCompactionStarted,
            json!({
                "after_model_call_index": 1,
                "active_context_tokens": 100,
                "auto_compact_token_limit": 90,
            }),
        ),
        agent_event(
            7,
            AgentEventKind::ModelCompactionCompleted,
            json!({
                "after_model_call_index": 1,
                "attempt": 1,
                "connection_generation": 1,
                "status": "completed",
                "duration_ns": 33,
                "time_to_first_event_ns": 3,
                "time_to_first_output_ns": 4,
                "usage": compaction_usage,
            }),
        ),
        agent_event(
            8,
            AgentEventKind::ModelCallStarted,
            json!({
                "call_index": 2,
                "model": "gpt-5.6-sol",
                "reasoning_mode": "summary",
                "effort": "high",
            }),
        ),
    ];
    let mut observation = AgentObservation::default();
    for event in &events {
        observation.observe(event).unwrap();
    }

    let selection = observation.select_result(None, BillingCompleteness::Complete);
    assert!(selection.used_lower_bound);
    assert!(selection.terminal_error.is_none());
    let mut outcome = super::AgentTurnOutcome {
        primary: None,
        result: selection.result,
        result_is_lower_bound: selection.used_lower_bound,
    };
    outcome.apply_lower_bound_duration(9_876_543);
    let result = outcome.result.unwrap();

    assert_eq!(result.billing_completeness, BillingCompleteness::Unknown);
    assert_eq!(result.metadata.status, AgentStatus::Cancelled);
    assert_eq!(result.metadata.duration_ns, 9_876_543);
    assert_eq!(result.metadata.duration_ms, 9);
    assert_eq!(result.model_calls, 2);
    assert_eq!(
        result.metadata.runtime_completeness,
        crate::MeasurementCompleteness::ObservedLowerBound
    );
    assert_eq!(result.tool_calls, 2);
    assert_eq!(result.usage.input_tokens, 250);
    assert_eq!(result.usage.cached_input_tokens, 110);
    assert_eq!(result.usage.cache_write_input_tokens, 25);
    assert_eq!(result.usage.output_tokens, 36);
    assert_eq!(result.usage.reasoning_output_tokens, 8);
    assert_eq!(result.usage.total_tokens, 286);
    assert_eq!(result.metadata.warmup_usage.input_tokens, 100);
    assert_eq!(result.metadata.warmup_usage.cached_input_tokens, 40);
    assert_eq!(result.metadata.warmup_usage.output_tokens, 20);
    assert_eq!(result.metadata.compactions, 1);
    assert_eq!(result.metadata.response_attempts, 4);
    assert_eq!(result.metadata.response_retries, 1);
    assert_eq!(result.cost_usd, None);
    assert_eq!(
        result.metadata.cost_status,
        CostStatus::UsageNotReported.as_str()
    );
    assert!(result.metadata.estimated_cost.is_none());
}

#[test]
fn timeout_reconstructs_observed_tool_and_transport_lower_bounds() {
    let events = [
        agent_event(
            1,
            AgentEventKind::RunStarted,
            json!({
                "mode": "openai_model",
                "model": "gpt-5.6-sol",
                "reasoning_mode": "summary",
                "effort": "high",
                "transport": "responses_websocket_v2",
                "orchestration": "agent",
                "websocket_url": "wss://example.invalid",
                "workspace": null,
                "instruction_bytes": 4,
            }),
        ),
        agent_event(2, AgentEventKind::ModelConnectionStarted, json!({})),
        agent_event(
            3,
            AgentEventKind::ModelConnectionCompleted,
            json!({"purpose": "initial", "duration_ns": 10}),
        ),
        agent_event(4, AgentEventKind::ModelAttemptStarted, json!({})),
        agent_event(
            5,
            AgentEventKind::ModelAttemptRetrying,
            json!({"delay_ns": 7}),
        ),
        agent_event(6, AgentEventKind::ModelAttemptStarted, json!({})),
        agent_event(7, AgentEventKind::ModelConnectionStarted, json!({})),
        agent_event(
            8,
            AgentEventKind::ModelConnectionFailed,
            json!({"duration_ns": 11}),
        ),
        agent_event(9, AgentEventKind::ModelConnectionStarted, json!({})),
        agent_event(
            10,
            AgentEventKind::ModelConnectionCompleted,
            json!({"purpose": "reconnect", "duration_ns": 12}),
        ),
        agent_event(
            11,
            AgentEventKind::ModelCallStarted,
            json!({
                "call_index": 1,
                "model": "gpt-5.6-sol",
                "reasoning_mode": "summary",
                "effort": "high",
            }),
        ),
        agent_event(
            12,
            AgentEventKind::ModelCallCompleted,
            json!({
                "call_index": 1,
                "model": "gpt-5.6-sol",
                "attempt": 2,
                "connection_generation": 1,
                "status": "completed",
                "duration_ns": 13,
                "time_to_first_event_ns": 2,
                "time_to_first_output_ns": 3,
                "tool_calls": 1,
                "usage": null,
            }),
        ),
        agent_event(13, AgentEventKind::ToolCall, json!({})),
        agent_event(
            14,
            AgentEventKind::ToolResult,
            json!({
                "call_id": "call-1",
                "tool": "shell",
                "status": "completed",
                "duration_ns": 42,
                "started_after_ns": null,
                "result": "done",
                "metadata": null,
            }),
        ),
        agent_event(
            15,
            AgentEventKind::ModelCallStarted,
            json!({
                "call_index": 2,
                "model": "gpt-5.6-sol",
                "reasoning_mode": "summary",
                "effort": "high",
            }),
        ),
    ];
    let mut observation = AgentObservation::default();
    for event in &events {
        observation.observe(event).unwrap();
    }

    let result = observation
        .select_result(None, BillingCompleteness::Unknown)
        .result
        .expect("run activity must produce a partial runtime snapshot");

    assert_eq!(
        result.metadata.runtime_completeness,
        crate::MeasurementCompleteness::ObservedLowerBound
    );
    assert_eq!(result.metadata.model_calls, 2);
    assert_eq!(result.metadata.tool_calls, 1);
    assert_eq!(result.metadata.connection_attempts, 3);
    assert_eq!(result.metadata.websocket_reconnects, 1);
    assert_eq!(result.metadata.response_attempts, 2);
    assert_eq!(result.metadata.response_retries, 1);
    assert_eq!(result.metadata.connection_duration_ns, 33);
    assert_eq!(result.metadata.retry_backoff_duration_ns, 7);
    assert_eq!(result.metadata.model_duration_ns, 13);
    assert_eq!(result.metadata.tool_work_duration_ns, 42);
    assert_eq!(result.metadata.tool_wall_duration_ns, 0);
    assert_eq!(
        result.metadata.cost_status,
        CostStatus::UsageNotReported.as_str()
    );
    assert!(result.metadata.estimated_cost.is_none());
}

#[test]
fn missing_usage_marks_terminal_unknown_and_terminal_metrics_take_precedence() {
    let reported_usage = provider_usage(10, 2, 1, 4, 1);
    let mut observation = AgentObservation::default();
    for event in [
        agent_event(
            1,
            AgentEventKind::RunStarted,
            json!({
                "mode": "openai_model",
                "model": "gpt-5.6-sol",
                "reasoning_mode": "summary",
                "effort": "medium",
                "transport": "websocket",
                "orchestration": "agent",
                "websocket_url": "wss://example.invalid",
                "workspace": null,
                "instruction_bytes": 4,
            }),
        ),
        agent_event(
            2,
            AgentEventKind::ModelCallStarted,
            json!({
                "call_index": 1,
                "model": "gpt-5.6-sol",
                "reasoning_mode": "summary",
                "effort": "medium",
            }),
        ),
        agent_event(
            3,
            AgentEventKind::ModelCallCompleted,
            json!({
                "call_index": 1,
                "model": "gpt-5.6-sol",
                "attempt": 1,
                "connection_generation": 1,
                "status": "completed",
                "duration_ns": 10,
                "time_to_first_event_ns": 1,
                "time_to_first_output_ns": 2,
                "tool_calls": 0,
                "usage": reported_usage,
            }),
        ),
        agent_event(
            4,
            AgentEventKind::ModelCallStarted,
            json!({
                "call_index": 2,
                "model": "gpt-5.6-sol",
                "reasoning_mode": "summary",
                "effort": "medium",
            }),
        ),
        agent_event(
            5,
            AgentEventKind::ModelCallCompleted,
            json!({
                "call_index": 2,
                "model": "gpt-5.6-sol",
                "attempt": 1,
                "connection_generation": 1,
                "status": "completed",
                "duration_ns": 10,
                "time_to_first_event_ns": 1,
                "time_to_first_output_ns": null,
                "tool_calls": 0,
                "usage": null,
            }),
        ),
    ] {
        observation.observe(&event).unwrap();
    }

    assert_eq!(observation.billable_in_flight, 0);
    assert_eq!(
        observation.billing_completeness(),
        BillingCompleteness::Unknown
    );
    let fallback = observation
        .select_result(None, BillingCompleteness::Complete)
        .result
        .unwrap();
    assert_eq!(fallback.billing_completeness, BillingCompleteness::Unknown);
    assert_eq!(fallback.model_calls, 2);
    assert_eq!(fallback.cost_usd, None);
    assert_eq!(
        fallback.metadata.cost_status,
        CostStatus::UsageNotReported.as_str()
    );

    let terminal = agent_event(6, AgentEventKind::RunCompleted, terminal_payload(77, 0.75));
    let terminal_result =
        observation.select_result(Some(&terminal), observation.billing_completeness());
    assert!(!terminal_result.used_lower_bound);
    assert!(terminal_result.terminal_error.is_none());
    let terminal_result = terminal_result.result.unwrap();
    assert_eq!(terminal_result.model_calls, 77);
    assert_eq!(terminal_result.usage.input_tokens, 7);
    assert_eq!(terminal_result.cost_usd, Some(0.75));
    assert_eq!(
        terminal_result.billing_completeness,
        BillingCompleteness::Unknown
    );

    let invalid_terminal = agent_event(7, AgentEventKind::RunCompleted, json!({"invalid": true}));
    let invalid = observation.select_result(Some(&invalid_terminal), BillingCompleteness::Complete);
    assert!(invalid.used_lower_bound);
    assert!(invalid.terminal_error.is_some());
    let invalid = invalid.result.unwrap();
    assert_eq!(invalid.metadata.status, AgentStatus::Completed);
    assert_eq!(invalid.cost_usd, None);

    let invalid_terminal = agent_event(8, AgentEventKind::RunFailed, json!({"invalid": true}));
    let invalid = observation.select_result(Some(&invalid_terminal), BillingCompleteness::Complete);
    assert_eq!(invalid.result.unwrap().metadata.status, AgentStatus::Failed);
}

#[test]
fn failed_model_call_without_a_sent_attempt_keeps_billing_complete() {
    let mut observation = AgentObservation::default();
    observation.observe_lifecycle(AgentEventKind::ModelCallStarted);
    observation.observe_lifecycle(AgentEventKind::ModelCallFailed);

    assert_eq!(observation.billable_in_flight, 0);
    assert_eq!(
        observation.billing_completeness(),
        BillingCompleteness::Complete
    );
}

#[test]
fn failed_sent_attempt_marks_billing_snapshot_unknown() {
    let mut observation = AgentObservation::default();
    observation
        .observe(&agent_event(
            1,
            AgentEventKind::ModelAttemptFailed,
            json!({"billing_uncertain": true}),
        ))
        .unwrap();

    assert_eq!(
        observation.billing_completeness(),
        BillingCompleteness::Unknown
    );
    assert_eq!(observation.billing_uncertain_response_attempts, 1);
}

#[test]
fn completed_terminal_with_an_uncertain_attempt_remains_a_lower_bound() {
    let observation = AgentObservation::default();
    let mut payload = terminal_payload(2, 0.25);
    payload["billing_uncertain_response_attempts"] = json!(1);
    let terminal = agent_event(1, AgentEventKind::RunCompleted, payload);

    let result = observation
        .select_result(Some(&terminal), BillingCompleteness::Complete)
        .result
        .expect("valid terminal event must produce a result");

    assert_eq!(result.billing_completeness, BillingCompleteness::Unknown);
    assert_eq!(result.metadata.billing_uncertain_response_attempts, 1);
}

#[test]
fn retained_terminal_metadata_requires_the_current_billing_field() {
    let mut payload = terminal_payload(1, 0.25);
    let fields = payload.as_object_mut().unwrap();
    fields.remove("billing_uncertain_response_attempts");
    fields.insert("accepted_abandoned_response_attempts".to_owned(), json!(2));

    let error = serde_json::from_value::<crate::AgentMetadata>(payload).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("billing_uncertain_response_attempts")
    );
}

fn provider_usage(
    input_tokens: u64,
    cached_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
) -> Usage {
    Usage {
        input_tokens,
        input_tokens_details: Some(InputTokenDetails {
            cached_tokens,
            cache_write_tokens,
        }),
        output_tokens,
        output_tokens_details: Some(OutputTokenDetails { reasoning_tokens }),
        total_tokens: input_tokens.saturating_add(output_tokens),
    }
}

fn agent_event(seq: u64, kind: AgentEventKind, payload: Value) -> AgentEvent {
    serde_json::from_value(json!({
        "protocol_version": 1,
        "request_id": "test-request",
        "seq": seq,
        "type": kind,
        "payload": payload,
    }))
    .unwrap()
}

fn terminal_payload(model_calls: u32, cost_usd: f64) -> Value {
    json!({
        "status": "completed",
        "model": "terminal-model",
        "effort": "high",
        "transport": "websocket",
        "orchestration": "agent",
        "runtime_completeness": "complete",
        "duration_ms": 5,
        "duration_ns": 5_000_000,
        "model_calls": model_calls,
        "steers": 0,
        "compactions": 0,
        "tool_calls": 0,
        "connection_attempts": 1,
        "websocket_reconnects": 0,
        "response_attempts": 1,
        "response_retries": 0,
        "billing_uncertain_response_attempts": 0,
        "connection_duration_ns": 1,
        "retry_backoff_duration_ns": 0,
        "model_duration_ns": 4,
        "warmup_duration_ns": 1,
        "tool_work_duration_ns": 0,
        "tool_wall_duration_ns": 0,
        "usage": {
            "input_tokens": 7,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 3,
            "reasoning_output_tokens": 1,
            "total_tokens": 10,
        },
        "warmup_usage": {
            "input_tokens": 0,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "total_tokens": 0,
        },
        "cost_usd": cost_usd,
        "cost_status": "estimated_from_usage",
    })
}

#[tokio::test(flavor = "current_thread")]
async fn expired_terminal_grace_keeps_shutdown_joined() {
    let (release_shutdown, shutdown_released) = tokio::sync::oneshot::channel();
    let shutdown_started = Arc::new(tokio::sync::Notify::new());
    let started = Arc::clone(&shutdown_started);
    let recovery = tokio::spawn(super::recover_timed_out_agent(
        Duration::ZERO,
        async move {
            started.notify_one();
            shutdown_released.await.unwrap();
            "joined"
        },
        std::future::pending::<()>(),
    ));

    shutdown_started.notified().await;
    tokio::task::yield_now().await;
    assert!(
        !recovery.is_finished(),
        "the recovery deadline must not detach resource shutdown"
    );
    release_shutdown.send(()).unwrap();
    let recovered = recovery.await.unwrap();

    assert!(recovered.grace_elapsed);
    assert!(recovered.terminal.is_none());
    assert_eq!(recovered.shutdown, "joined");
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_snapshot_survives_while_shutdown_remains_mandatory() {
    let (release_shutdown, shutdown_released) = tokio::sync::oneshot::channel();
    let recovery = tokio::spawn(super::recover_timed_out_agent(
        Duration::ZERO,
        async move {
            shutdown_released.await.unwrap();
            "joined"
        },
        std::future::ready("terminal"),
    ));

    tokio::task::yield_now().await;
    assert!(
        !recovery.is_finished(),
        "a retained terminal must not let the caller skip shutdown"
    );
    release_shutdown.send(()).unwrap();
    let recovered = recovery.await.unwrap();

    assert_eq!(recovered.terminal, Some("terminal"));
    assert_eq!(recovered.shutdown, "joined");
}

async fn run_timed_out_attempt<V, F>(make_verifier: F) -> TimeoutRun
where
    V: AttemptVerifier + 'static,
    F: Fn() -> V + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let warmup = socket.next().await.unwrap().unwrap();
        assert!(warmup.is_text());
        socket
            .send(Message::Text(
                json!({
                    "type": "response.completed",
                    "response": { "id": "resp-warmup", "usage": null }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let generation = socket.next().await.unwrap().unwrap();
        assert!(generation.is_text());
        while socket.next().await.is_some() {}
    });
    let live_resources = Arc::new(AtomicUsize::new(0));
    let openai = OpenAi::builder("test")
        .websocket_url(endpoint)
        .build()
        .unwrap();
    let tool_resources = Arc::clone(&live_resources);
    let nanocodex = Nanocodex::builder(openai).tools_factory(move |_agent| {
        tool_resources.fetch_add(1, Ordering::AcqRel);
        Tools::builder()
            .without_defaults()
            .provider(AttemptResourceProvider {
                live_resources: Arc::clone(&tool_resources),
            })
            .build()
    });
    let output = tempdir().unwrap();
    let verifier_resources = Arc::clone(&live_resources);
    let evaluator = Evaluator::new_builder(nanocodex)
        .output_directory(output.path())
        .attempt_agent(move |_attempt, builder| {
            Ok::<_, Infallible>(
                AttemptAgent::new(builder).verifier(ResourceCheckedVerifier {
                    inner: make_verifier(),
                    live_resources: Arc::clone(&verifier_resources),
                }),
            )
        })
        .build()
        .unwrap();
    // Leave enough headroom for a loaded parallel test runner to complete
    // the mock warmup before exercising the intentionally stalled model
    // call.
    let (_task_directory, task) = task_with_agent_timeout(0.5);
    let run = evaluator.task(task);
    let mut event_stream = run.events().subscribe();
    let recorder = Harbor::new(&evaluator)
        .unwrap()
        .record(run.events().subscribe())
        .unwrap();

    let outcome = run
        .await
        .expect("an accepted timeout must return a terminal outcome");
    let mut terminal_events = 0;
    while terminal_events == 0 {
        let event = event_stream
            .recv()
            .await
            .unwrap()
            .expect("the evaluator must remain open");
        terminal_events += usize::from(matches!(
            event.kind,
            EvalEventKind::Completed(_) | EvalEventKind::Failed(_)
        ));
    }
    while let Ok(Ok(Some(event))) = timeout(Duration::from_millis(5), event_stream.recv()).await {
        terminal_events += usize::from(matches!(
            event.kind,
            EvalEventKind::Completed(_) | EvalEventKind::Failed(_)
        ));
    }
    let job = recorder.finish_all(1).await.unwrap();
    let aggregate = job.aggregate_dataset().unwrap();
    let trial: Value = serde_json::from_slice(
        &fs::read(
            job.directory()
                .join(outcome.trial_name())
                .join("result.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let trajectory: Value = serde_json::from_slice(
        &fs::read(
            job.directory()
                .join(outcome.trial_name())
                .join("agent/trajectory.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let job_result =
        serde_json::from_slice(&fs::read(job.directory().join("result.json")).unwrap()).unwrap();
    server.await.unwrap();

    TimeoutRun {
        outcome,
        trial,
        trajectory,
        job: job_result,
        aggregate,
        terminal_events,
        live_resources: live_resources.load(Ordering::Acquire),
    }
}

fn task_with_agent_timeout(timeout_seconds: f64) -> (tempfile::TempDir, Task) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting");
    let destination = tempdir().unwrap();
    for directory in ["environment", "tests"] {
        fs::create_dir_all(destination.path().join(directory)).unwrap();
    }
    for file in [
        "instruction.md",
        "environment/Dockerfile",
        "environment/README.md",
        "tests/test.sh",
    ] {
        fs::copy(source.join(file), destination.path().join(file)).unwrap();
    }
    let manifest = fs::read_to_string(source.join("task.toml"))
        .unwrap()
        .replace(
            "timeout_sec = 300.0",
            &format!("timeout_sec = {timeout_seconds}"),
        );
    fs::write(destination.path().join("task.toml"), manifest).unwrap();
    let task = Task::load(destination.path()).unwrap();
    (destination, task)
}
