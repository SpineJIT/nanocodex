use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt as _,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    AtifTrajectory, BillingCompleteness, EvalArtifacts, EvalCleanup, EvalEnvironment, EvalEvent,
    EvalEventAttempt, EvalEventKind, EvalEvents, EvalException, EvalExceptionKind, EvalFailure,
    EvalFailureTiming, EvalOutcome, Evaluator, PhaseTiming, Sweep, Task,
};
use chrono::{DateTime, Utc};
use nanocodex_agent::{Nanocodex, OpenAi};
use serde::Deserialize;
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::broadcast;
use uuid::Uuid;

use super::{
    Harbor, HarborArtifacts, HarborError, HarborJob, HarborRecorder, compute_pass_at_k_for_tasks,
    pass_at_k_for_task, retained_lifecycle_classification,
};

#[derive(Deserialize)]
struct TrialResult {
    exception_info: Option<ExceptionInfo>,
}

#[derive(Deserialize)]
struct ExceptionInfo {
    exception_type: String,
    exception_message: String,
}

#[derive(Deserialize)]
struct JobResult {
    finished_at: Option<DateTime<Utc>>,
    n_total_trials: usize,
    stats: JobStats,
}

#[derive(Deserialize)]
struct JobStats {
    #[serde(rename = "n_completed_trials")]
    completed: usize,
    #[serde(rename = "n_errored_trials")]
    errored: usize,
    #[serde(rename = "n_running_trials")]
    running: usize,
    #[serde(rename = "n_pending_trials")]
    pending: usize,
}

#[test]
fn explicit_exception_precedes_the_outcome_lifecycle_axes() {
    assert_eq!(
        retained_lifecycle_classification(EvalOutcome::InfrastructureError, Some("CleanupError"),),
        (false, false)
    );
    assert_eq!(
        retained_lifecycle_classification(EvalOutcome::SafetyRefusal, Some("VerifierError"),),
        (true, false)
    );
    assert_eq!(
        retained_lifecycle_classification(EvalOutcome::Passed, Some("AgentTimeoutError")),
        (true, false)
    );
    assert_eq!(
        retained_lifecycle_classification(EvalOutcome::SafetyRefusal, None),
        (true, true)
    );
}

#[test]
fn terminal_result_is_committed_only_after_artifacts_and_lock() {
    let output = tempdir().unwrap();
    let job = output.path().join("job");
    let trial = job.join("trial");
    let artifact = trial.join("agent/trajectory.json");
    let lock = trial.join("lock.json");
    let result = trial.join("result.json");
    HarborArtifacts::write_file(&artifact, b"{}\n").unwrap();

    let error = HarborArtifacts::write_terminal_json(
        &result,
        &json!({"status": "completed"}),
        &[artifact.as_path(), lock.as_path()],
        &job,
    )
    .unwrap_err();

    assert!(matches!(error, HarborError::MissingTerminalPrerequisite(path) if path == lock));
    assert!(!result.exists());

    HarborArtifacts::write_file(&lock, b"{}\n").unwrap();
    let mut directory_syncs = Vec::new();
    HarborArtifacts::write_terminal_json_with_sync(
        &result,
        &json!({"status": "completed"}),
        &[artifact.as_path(), lock.as_path()],
        &job,
        |directory| {
            directory_syncs.push((directory.to_path_buf(), result.exists()));
            Ok(())
        },
    )
    .unwrap();

    let retained: serde_json::Value = serde_json::from_slice(&fs::read(result).unwrap()).unwrap();
    assert_eq!(retained["status"], "completed");
    let terminal_trial_sync = directory_syncs
        .iter()
        .position(|(directory, result_exists)| directory == &trial && *result_exists)
        .unwrap();
    let terminal_job_sync = directory_syncs
        .iter()
        .position(|(directory, result_exists)| directory == &job && *result_exists)
        .unwrap();
    assert_eq!(
        directory_syncs.last(),
        Some(&(output.path().to_path_buf(), true))
    );
    assert!(terminal_trial_sync < terminal_job_sync);
    assert!(terminal_job_sync < directory_syncs.len() - 1);
}

#[test]
fn atomic_write_replaces_target_without_leaving_temporary_files() {
    let output = tempdir().unwrap();
    let target = output.path().join("result.json");

    HarborArtifacts::atomic_write(&target, b"first\n").unwrap();
    HarborArtifacts::atomic_write(&target, b"second\n").unwrap();

    assert_eq!(fs::read(&target).unwrap(), b"second\n");
    let entries = fs::read_dir(output.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(entries, [target]);
}

#[test]
fn attach_rebuilds_stale_job_stats_from_durable_trials() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let sweep = Sweep::builder()
        .task(task.clone())
        .trials(2)
        .agent(
            "default",
            Nanocodex::builder(OpenAi::new("test-key").unwrap()),
        )
        .unwrap()
        .build()
        .unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .fresh_run(sweep)
        .build()
        .unwrap();

    Harbor::new(&eval).unwrap();
    let stale: JobResult =
        serde_json::from_slice(&fs::read(eval.directory().join("result.json")).unwrap()).unwrap();
    assert_eq!(stale.stats.completed, 0);
    write_retained_trial(
        eval.directory(),
        eval.id(),
        &task,
        "default",
        1,
        Some(1.0),
        false,
    );

    Harbor::new(&eval).unwrap();
    let rebuilt: serde_json::Value =
        serde_json::from_slice(&fs::read(eval.directory().join("result.json")).unwrap()).unwrap();
    assert_eq!(rebuilt["n_total_trials"], 2);
    assert_eq!(rebuilt["stats"]["n_completed_trials"], 1);
    assert_eq!(rebuilt["stats"]["n_pending_trials"], 1);
    assert_eq!(rebuilt["stats"]["n_input_tokens"], 10);
    assert_eq!(rebuilt["stats"]["n_cache_tokens"], 4);
    assert_eq!(rebuilt["stats"]["n_output_tokens"], 3);
    assert_eq!(rebuilt["stats"]["cost_usd"], 0.25);
    assert_eq!(
        rebuilt["stats"]["evals"]["nanocodex__gpt-test__nanocodex/local"]["n_trials"],
        1
    );
    assert_eq!(
        rebuilt["stats"]["evals"]["nanocodex__gpt-test__nanocodex/local"]["reward_stats"]["reward"]
            ["1.0"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let rebuilt_lock: serde_json::Value =
        serde_json::from_slice(&fs::read(eval.directory().join("lock.json")).unwrap()).unwrap();
    assert_eq!(rebuilt_lock["trials"].as_array().unwrap().len(), 1);
}

#[test]
fn aggregate_reconstructs_every_durable_trial_with_sweep_provenance() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let sweep = Sweep::builder()
        .task(task.clone())
        .trials(2)
        .agent(
            "recipe__variant",
            Nanocodex::builder(OpenAi::new("test-key").unwrap()),
        )
        .unwrap()
        .build()
        .unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .fresh_run(sweep)
        .build()
        .unwrap();
    let first = write_retained_trial(
        eval.directory(),
        eval.id(),
        &task,
        "recipe__variant",
        1,
        Some(1.0),
        false,
    );
    let second = write_retained_trial(
        eval.directory(),
        eval.id(),
        &task,
        "recipe__variant",
        2,
        None,
        false,
    );
    let job = HarborJob {
        id: eval.id(),
        directory: eval.directory().to_path_buf(),
    };

    let aggregate = job.aggregate_dataset().unwrap();

    assert_eq!(aggregate.attempts.len(), 2);
    assert_eq!(aggregate.attempts[0].attempt_id, first);
    assert_eq!(aggregate.attempts[0].configuration.id, "recipe__variant");
    assert_eq!(aggregate.attempts[0].repetition, 1);
    assert!(aggregate.attempts[0].passed);
    assert_eq!(aggregate.attempts[0].cost_usd, Some(0.25));
    assert_eq!(
        aggregate.attempts[0].task.package_digest_schema,
        super::PACKAGE_DIGEST_SCHEMA
    );
    assert_eq!(
        aggregate.attempts[0].task.image_reference.as_deref(),
        Some("alpine:3.21")
    );
    assert_eq!(
        aggregate.attempts[0].task.verifier.script.as_deref(),
        Some(Path::new("tests/test.sh"))
    );
    assert_eq!(aggregate.attempts[0].configuration.model, "gpt-test");
    assert_eq!(aggregate.attempts[0].configuration.reasoning_effort, "high");
    assert_eq!(
        aggregate.attempts[0].configuration.service_tier.as_deref(),
        Some("standard")
    );
    let usage = aggregate.attempts[0].usage.as_ref().unwrap();
    assert_eq!(usage.combined.input_tokens, 12);
    assert_eq!(usage.combined.cached_input_tokens, 6);
    assert_eq!(usage.combined.cache_write_input_tokens, 2);
    assert_eq!(usage.combined.output_tokens, 3);
    assert_eq!(usage.combined.reasoning_output_tokens, 1);
    assert_eq!(
        aggregate.attempts[0]
            .estimated_cost
            .as_ref()
            .map(|cost| cost.cache_write_input().decimal()),
        Some("0.03".to_owned())
    );
    assert_eq!(aggregate.attempts[0].verifier.rewards["reward"], 1.0);
    assert_eq!(aggregate.attempts[1].attempt_id, second);
    assert_eq!(aggregate.attempts[1].configuration.id, "recipe__variant");
    assert_eq!(aggregate.attempts[1].repetition, 2);
    assert!(!aggregate.attempts[1].passed);
    assert!(!aggregate.attempts[1].scored);
    assert_eq!(
        aggregate.attempts[1].outcome,
        EvalOutcome::InfrastructureError
    );
    assert_eq!(aggregate.attempts[1].cost_usd, None);
    assert_eq!(
        aggregate.attempts[1].exception_kind,
        Some(EvalExceptionKind::Agent)
    );
    assert_eq!(aggregate.configurations.len(), 1);
    assert_eq!(aggregate.configurations[0].success.samples, 2);
    assert_eq!(aggregate.configurations[0].success.successes, 1);
    assert_eq!(
        aggregate.configurations[0]
            .verifier_conditioned_success
            .samples,
        1
    );
    assert_eq!(
        aggregate.configurations[0]
            .verifier_conditioned_success
            .successes,
        1
    );
    assert_eq!(aggregate.configurations[0].unscored_attempts, 1);
    assert_eq!(aggregate.configurations[0].tokens.output_tokens.samples, 1);
    assert_eq!(
        aggregate.configurations[0].tokens.output_tokens.mean,
        Some(3.0)
    );
    assert_eq!(
        aggregate.configurations[0]
            .observed_cost_components_lower_bound_usd
            .cache_write_input_usd
            .mean,
        Some(0.03)
    );
    assert_eq!(
        aggregate.configurations[0].exceptions[&EvalExceptionKind::Agent],
        1
    );
}

#[test]
fn scored_cleanup_failure_stays_in_harbor_denominators() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let sweep = Sweep::builder()
        .task(task.clone())
        .agent(
            "default",
            Nanocodex::builder(OpenAi::new("test-key").unwrap()),
        )
        .unwrap()
        .build()
        .unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .fresh_run(sweep)
        .build()
        .unwrap();
    write_retained_trial(
        eval.directory(),
        eval.id(),
        &task,
        "default",
        1,
        Some(1.0),
        true,
    );
    let trial = fs::read_dir(eval.directory())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .unwrap()
        .path();
    let result_path = trial.join("result.json");
    let mut result: serde_json::Value =
        serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
    result["outcome"] = json!("infrastructure_error");
    result["exception_info"] = json!({
        "exception_type": "CleanupError",
    });
    HarborArtifacts::write_json(&result_path, &result).unwrap();

    Harbor::new(&eval).unwrap();
    let rebuilt: serde_json::Value =
        serde_json::from_slice(&fs::read(eval.directory().join("result.json")).unwrap()).unwrap();
    let eval_stats = &rebuilt["stats"]["evals"]["nanocodex__gpt-test__nanocodex/local"];
    assert_eq!(rebuilt["stats"]["n_errored_trials"], 0);
    assert_eq!(rebuilt["stats"]["n_cleanup_failed_trials"], 1);
    assert_eq!(eval_stats["n_trials"], 1);
    assert_eq!(eval_stats["n_errors"], 0);
    assert_eq!(eval_stats["n_cleanup_failures"], 1);

    let aggregate = HarborJob {
        id: eval.id(),
        directory: eval.directory().to_path_buf(),
    }
    .aggregate_dataset()
    .unwrap();
    assert_eq!(aggregate.configurations[0].success.samples, 1);
    assert_eq!(aggregate.configurations[0].success.successes, 1);
    assert_eq!(aggregate.configurations[0].cleanup_failures, 1);
    assert!(aggregate.attempts[0].passed);
    assert!(!aggregate.attempts[0].errored);
    assert!(aggregate.attempts[0].cleanup_failed);
}

#[test]
fn scored_timeout_cost_is_reloaded_as_an_observed_lower_bound() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let sweep = Sweep::builder()
        .task(task.clone())
        .agent(
            "default",
            Nanocodex::builder(OpenAi::new("test-key").unwrap()),
        )
        .unwrap()
        .build()
        .unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .fresh_run(sweep)
        .build()
        .unwrap();
    write_retained_trial(
        eval.directory(),
        eval.id(),
        &task,
        "default",
        1,
        Some(1.0),
        false,
    );
    let trial = fs::read_dir(eval.directory())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .unwrap()
        .path();
    let result_path = trial.join("result.json");
    let mut result: serde_json::Value =
        serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
    result["outcome"] = json!("agent_timeout");
    result["scored"] = json!(true);
    result["agent_result"]["billing_completeness"] = json!("unknown");
    result["exception_info"] = json!({
        "exception_type": "AgentTimeoutError",
        "exception_message": "deterministic timeout",
        "exception_traceback": "deterministic timeout",
        "occurred_at": Utc::now(),
    });
    HarborArtifacts::write_json(&result_path, &result).unwrap();

    Harbor::new(&eval).unwrap();
    let rebuilt: serde_json::Value =
        serde_json::from_slice(&fs::read(eval.directory().join("result.json")).unwrap()).unwrap();
    assert_eq!(rebuilt["stats"]["n_billing_unknown_trials"], 1);
    assert!(
        rebuilt["stats"]["cost_usd"].is_null(),
        "the unqualified Harbor job cost must remain exact-only"
    );

    let aggregate = HarborJob {
        id: eval.id(),
        directory: eval.directory().to_path_buf(),
    }
    .aggregate_dataset()
    .unwrap();
    assert_eq!(aggregate.attempts[0].cost_usd, Some(0.25));
    assert_eq!(aggregate.attempts[0].outcome, EvalOutcome::AgentTimeout);
    assert!(aggregate.attempts[0].errored);
    assert_eq!(
        aggregate.attempts[0].billing_completeness,
        Some(BillingCompleteness::Unknown)
    );
    assert_eq!(aggregate.configurations[0].cost_usd.samples, 0);
    assert_eq!(
        aggregate.configurations[0]
            .observed_cost_lower_bound_usd
            .samples,
        1
    );
    assert_eq!(
        aggregate.configurations[0]
            .observed_cost_lower_bound_usd
            .mean,
        Some(0.25)
    );
    assert_eq!(aggregate.configurations[0].billing_unknown_attempts, 1);
}

#[test]
fn scored_timeout_without_agent_metrics_resumes_and_aggregates_independent_axes() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let sweep = Sweep::builder()
        .task(task.clone())
        .agent(
            "default",
            Nanocodex::builder(OpenAi::new("test-key").unwrap()),
        )
        .unwrap()
        .build()
        .unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .fresh_run(sweep)
        .build()
        .unwrap();
    write_retained_trial(
        eval.directory(),
        eval.id(),
        &task,
        "default",
        1,
        Some(1.0),
        false,
    );
    let trial = fs::read_dir(eval.directory())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .unwrap()
        .path();
    let result_path = trial.join("result.json");
    let mut result: serde_json::Value =
        serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
    result["outcome"] = json!("agent_timeout");
    result["scored"] = json!(true);
    result["agent_result"] = serde_json::Value::Null;
    result["exception_info"] = json!({
        "exception_type": "AgentTimeoutError",
        "exception_message": "deterministic timeout",
        "exception_traceback": "deterministic timeout",
        "occurred_at": Utc::now(),
    });
    HarborArtifacts::write_json(&result_path, &result).unwrap();

    assert_eq!(eval.remaining_attempts().unwrap(), 0);
    Harbor::new(&eval).unwrap();
    let rebuilt: serde_json::Value =
        serde_json::from_slice(&fs::read(eval.directory().join("result.json")).unwrap()).unwrap();
    let eval_stats = rebuilt["stats"]["evals"]
        .as_object()
        .and_then(|evals| evals.values().next())
        .unwrap();
    assert_eq!(rebuilt["stats"]["n_completed_trials"], 1);
    assert_eq!(rebuilt["stats"]["n_errored_trials"], 1);
    assert_eq!(rebuilt["stats"]["n_billing_missing_trials"], 1);
    assert!(rebuilt["stats"]["cost_usd"].is_null());
    assert_eq!(eval_stats["n_trials"], 1);
    assert_eq!(eval_stats["n_errors"], 1);
    assert_eq!(
        eval_stats["reward_stats"]["reward"]["1.0"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let aggregate = HarborJob {
        id: eval.id(),
        directory: eval.directory().to_path_buf(),
    }
    .aggregate_dataset()
    .unwrap();
    assert_eq!(aggregate.attempts.len(), 1);
    assert!(aggregate.attempts[0].scored);
    assert!(aggregate.attempts[0].passed);
    assert!(aggregate.attempts[0].errored);
    assert_eq!(aggregate.attempts[0].outcome, EvalOutcome::AgentTimeout);
    assert_eq!(aggregate.attempts[0].cost_usd, None);
    assert_eq!(aggregate.attempts[0].billing_completeness, None);
    assert!(aggregate.attempts[0].billing_snapshot_missing);
    assert_eq!(aggregate.configurations[0].success.samples, 1);
    assert_eq!(aggregate.configurations[0].success.successes, 1);
    assert_eq!(aggregate.configurations[0].errored_attempts, 1);
    assert_eq!(aggregate.configurations[0].billing_missing_attempts, 1);
    assert_eq!(aggregate.configurations[0].cost_usd.samples, 0);
    assert_eq!(
        aggregate.configurations[0]
            .observed_cost_lower_bound_usd
            .samples,
        0
    );
}

#[test]
fn pass_at_k_matches_harbors_unbiased_estimator() {
    assert!((pass_at_k_for_task(5, 2, 2) - 0.7).abs() < f64::EPSILON);
    let unscored_reward = retained_binary_result(false, Some(1.0));
    let scored_without_reward = retained_binary_result(true, None);
    let mut explicit_scored_timeout = retained_binary_result(true, Some(1.0));
    explicit_scored_timeout.outcome = EvalOutcome::AgentTimeout;
    let mut verifier_with_exception = retained_binary_result(false, Some(1.0));
    verifier_with_exception.exception_info = Some(super::RetainedHarborExceptionInfo {
        exception_type: "AgentTimeoutError".to_owned(),
    });
    assert_eq!(super::harbor_binary_success(&unscored_reward), Some(0));
    assert_eq!(
        super::harbor_binary_success(&scored_without_reward),
        Some(0)
    );
    assert_eq!(
        super::harbor_binary_success(&explicit_scored_timeout),
        Some(1)
    );
    assert_eq!(
        super::harbor_binary_success(&verifier_with_exception),
        Some(0)
    );

    let tasks = BTreeMap::from([
        ("sometimes".to_owned(), vec![1, 0, 0, 0, 0]),
        ("always".to_owned(), vec![1, 1, 1, 1, 1]),
    ]);
    let pass_at_k = compute_pass_at_k_for_tasks(&tasks).unwrap();

    assert_eq!(pass_at_k.keys().copied().collect::<Vec<_>>(), [2, 4, 5]);
    assert!((pass_at_k[&2] - 0.7).abs() < f64::EPSILON);
    assert!((pass_at_k[&4] - 0.9).abs() < f64::EPSILON);
    assert!((pass_at_k[&5] - 1.0).abs() < f64::EPSILON);
}

fn retained_binary_result(scored: bool, reward: Option<f64>) -> super::RetainedHarborTrialResult {
    let outcome = if scored && reward.is_some_and(|reward| reward > 0.0) {
        EvalOutcome::Passed
    } else if scored {
        EvalOutcome::VerifierFailed
    } else {
        EvalOutcome::InfrastructureError
    };
    serde_json::from_value(json!({
        "id": Uuid::now_v7(),
        "task_name": "terminal-bench/test",
        "trial_name": "test__default__001__fixture",
        "source": "nanocodex/local",
        "task_checksum": "fixture-checksum",
        "outcome": outcome,
        "scored": scored,
        "cleanup": EvalCleanup::default(),
        "config": {
            "environment": {
                "kwargs": {
                    "backend": "native",
                },
            },
        },
        "agent_info": {
            "name": "nanocodex",
            "version": "test",
            "model_info": {
                "name": "gpt-test",
            },
        },
        "verifier_result": reward.map(|reward| json!({
            "exit_code": 0,
            "rewards": {
                "reward": reward,
            },
        })),
    }))
    .unwrap()
}

#[test]
fn trial_lock_keeps_harbors_hash_separate_from_internal_materialization_identity() {
    let task = write_greeting_task();
    let task_checksum = super::directory_hash(task.root()).unwrap();
    let task_content_hash = super::packager_content_hash(task.root()).unwrap();
    let lock = super::HarborTrialLock::new(
        &task,
        "gpt-test",
        "high",
        &task_content_hash,
        task.content_digest(),
        EvalEnvironment::Native,
    );

    let mut retained = serde_json::to_value(&lock).unwrap();
    assert_eq!(
        retained["task"]["digest"],
        format!("sha256:{task_content_hash}")
    );
    assert_ne!(task_checksum, task_content_hash);
    assert_eq!(
        retained["nanocodex"]["materialization_digest"],
        format!("sha256:{}", task.content_digest())
    );
    assert_eq!(
        retained["nanocodex"]["materialization_digest_schema"],
        super::PACKAGE_DIGEST_SCHEMA
    );
    assert_ne!(
        retained["task"]["digest"],
        retained["nanocodex"]["materialization_digest"]
    );
    serde_json::from_value::<super::HarborTrialLock>(retained.clone()).unwrap();

    retained.as_object_mut().unwrap().remove("nanocodex");
    assert!(serde_json::from_value::<super::HarborTrialLock>(retained).is_err());
}

#[tokio::test]
async fn finite_job_moves_one_trial_from_pending_to_running_to_completed() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let sweep = test_sweep(task.clone(), 2);
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .fresh_run(sweep)
        .build()
        .unwrap();
    let (events, recorder) = test_recorder(&eval);
    let attempt_id = Uuid::now_v7();
    let trial_name = finite_trial_name(&task, "default", 1, attempt_id);

    events
        .send(started_event(&eval, &task, attempt_id, &trial_name))
        .unwrap();
    let running = wait_for_job_state(eval.directory(), (2, 0, 1, 1)).await;
    assert!(running.finished_at.is_none());

    events
        .send(failed_event(&eval, task, attempt_id, trial_name, 2))
        .unwrap();
    let terminal = wait_for_job_state(eval.directory(), (2, 1, 0, 1)).await;
    assert!(terminal.finished_at.is_none());

    let job = recorder.finish_all(1).await.unwrap();
    let final_result = read_job(job.directory());
    assert_job_state(&final_result, (2, 1, 0, 1));
    assert!(final_result.finished_at.is_none());
}

#[tokio::test]
async fn unplanned_job_counts_running_attempt_in_observed_total() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .build()
        .unwrap();
    let (events, recorder) = test_recorder(&eval);
    let attempt_id = Uuid::now_v7();
    let trial_name = format!("write-greeting__{}", attempt_id.simple());

    events
        .send(started_event(&eval, &task, attempt_id, &trial_name))
        .unwrap();
    let running = wait_for_job_state(eval.directory(), (1, 0, 1, 0)).await;
    assert!(running.finished_at.is_none());

    events
        .send(failed_event(&eval, task, attempt_id, trial_name, 2))
        .unwrap();
    let terminal = wait_for_job_state(eval.directory(), (1, 1, 0, 0)).await;
    assert!(terminal.finished_at.is_some());

    let job = recorder.finish_all(1).await.unwrap();
    let final_result = read_job(job.directory());
    assert_job_state(&final_result, (1, 1, 0, 0));
    assert!(final_result.finished_at.is_some());
}

#[tokio::test]
async fn duplicate_active_start_preserves_artifacts_and_live_stats() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .build()
        .unwrap();
    let (events, recorder) = test_recorder(&eval);
    let attempt_id = Uuid::now_v7();
    let trial_name = format!("write-greeting__{}", attempt_id.simple());
    let started = started_event(&eval, &task, attempt_id, &trial_name);
    events.send(Arc::clone(&started)).unwrap();
    wait_for_job_state(eval.directory(), (1, 0, 1, 0)).await;

    let trial = eval.directory().join(&trial_name);
    let input = trial.join("agent/input.jsonl");
    let event_log = trial.join("agent/events.jsonl");
    fs::write(&event_log, b"must survive duplicate start\n").unwrap();
    let job_result = eval.directory().join("result.json");
    let before = [
        file_snapshot(&job_result),
        file_snapshot(&input),
        file_snapshot(&event_log),
    ];

    events.send(started).unwrap();
    wait_for_recorder_stop(&recorder).await;
    let error = recorder.finish_all(1).await.unwrap_err();
    assert!(matches!(
        error,
        HarborError::DuplicateAttempt(found) if found == attempt_id
    ));
    assert_eq!(
        before,
        [
            file_snapshot(&job_result),
            file_snapshot(&input),
            file_snapshot(&event_log),
        ]
    );
    assert_job_state(&read_job(eval.directory()), (1, 0, 1, 0));
}

#[tokio::test]
async fn start_replay_after_terminal_does_not_resurrect_or_rewrite_attempt() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .build()
        .unwrap();
    let (events, recorder) = test_recorder(&eval);
    let attempt_id = Uuid::now_v7();
    let trial_name = format!("write-greeting__{}", attempt_id.simple());
    events
        .send(failed_event(
            &eval,
            task.clone(),
            attempt_id,
            trial_name.clone(),
            1,
        ))
        .unwrap();
    wait_for_job_state(eval.directory(), (1, 1, 0, 0)).await;

    let trial = eval.directory().join(&trial_name);
    let job_result = eval.directory().join("result.json");
    let trial_result = trial.join("result.json");
    let event_log = trial.join("agent/events.jsonl");
    let before = [
        file_snapshot(&job_result),
        file_snapshot(&trial_result),
        file_snapshot(&event_log),
    ];

    events
        .send(started_event(&eval, &task, attempt_id, &trial_name))
        .unwrap();
    wait_for_recorder_stop(&recorder).await;
    let error = recorder.finish_all(1).await.unwrap_err();
    assert!(matches!(
        error,
        HarborError::DuplicateAttempt(found) if found == attempt_id
    ));
    assert_eq!(
        before,
        [
            file_snapshot(&job_result),
            file_snapshot(&trial_result),
            file_snapshot(&event_log),
        ]
    );
    assert_job_state(&read_job(eval.directory()), (1, 1, 0, 0));
}

#[tokio::test]
async fn duplicate_terminal_is_rejected_before_failure_fallback_rewrites_artifacts() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .build()
        .unwrap();
    let (events, recorder) = test_recorder(&eval);
    let attempt_id = Uuid::now_v7();
    let trial_name = format!("write-greeting__{}", attempt_id.simple());
    let terminal = failed_event(&eval, task, attempt_id, trial_name.clone(), 1);
    events.send(Arc::clone(&terminal)).unwrap();
    wait_for_job_state(eval.directory(), (1, 1, 0, 0)).await;

    let trial = eval.directory().join(&trial_name);
    let job_result = eval.directory().join("result.json");
    let trial_result = trial.join("result.json");
    let event_log = trial.join("agent/events.jsonl");
    let before = [
        file_snapshot(&job_result),
        file_snapshot(&trial_result),
        file_snapshot(&event_log),
    ];

    events.send(terminal).unwrap();
    wait_for_recorder_stop(&recorder).await;
    let error = recorder.finish_all(1).await.unwrap_err();
    assert!(matches!(
        error,
        HarborError::DuplicateTerminal(found) if found == attempt_id
    ));
    assert_eq!(
        before,
        [
            file_snapshot(&job_result),
            file_snapshot(&trial_result),
            file_snapshot(&event_log),
        ]
    );
    assert_job_state(&read_job(eval.directory()), (1, 1, 0, 0));
}

#[tokio::test]
async fn finish_propagates_recorder_write_failure_after_finish_channel_closes() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .build()
        .unwrap();
    let (events, recorder) = test_recorder(&eval);
    let job_result = eval.directory().join("result.json");
    fs::remove_file(&job_result).unwrap();
    fs::create_dir(&job_result).unwrap();
    let attempt_id = Uuid::now_v7();
    let trial_name = format!("write-greeting__{}", attempt_id.simple());

    events
        .send(started_event(&eval, &task, attempt_id, &trial_name))
        .unwrap();
    wait_for_recorder_stop(&recorder).await;
    let error = recorder.finish(Vec::new()).await.unwrap_err();
    assert!(matches!(error, HarborError::Io(_)));
}

#[tokio::test]
async fn resumed_job_discards_stale_running_count_and_rebuilds_durable_state() {
    let output = tempdir().unwrap();
    let task = write_greeting_task();
    let sweep = test_sweep(task.clone(), 2);
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .fresh_run(sweep.clone())
        .build()
        .unwrap();
    let job_id = eval.id();
    let job_directory = eval.directory().to_path_buf();
    let (events, recorder) = test_recorder(&eval);
    let active_id = Uuid::now_v7();
    let active_name = finite_trial_name(&task, "default", 2, active_id);
    events
        .send(started_event(&eval, &task, active_id, &active_name))
        .unwrap();
    wait_for_job_state(eval.directory(), (2, 0, 1, 1)).await;
    drop(recorder);
    drop(events);
    tokio::task::yield_now().await;

    write_retained_trial(
        eval.directory(),
        eval.id(),
        &task,
        "default",
        1,
        Some(1.0),
        false,
    );
    drop(eval);

    let resumed = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .resume_incomplete(sweep.clone())
        .build()
        .unwrap();
    assert!(resumed.resumed());
    assert_eq!(resumed.id(), job_id);
    assert_eq!(resumed.directory(), job_directory);

    Harbor::new(&resumed).unwrap();
    let rebuilt = read_job(resumed.directory());
    assert_job_state(&rebuilt, (2, 1, 0, 1));
    assert!(rebuilt.finished_at.is_none());
}

#[test]
fn finite_job_records_pending_trials_before_execution() {
    let output = tempdir().unwrap();
    let task = Task::load(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
    )
    .unwrap();
    let sweep = Sweep::builder()
        .task(task)
        .trials(2)
        .agent(
            "default",
            Nanocodex::builder(OpenAi::new("test-key").unwrap()),
        )
        .unwrap()
        .build()
        .unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .fresh_run(sweep)
        .build()
        .unwrap();

    Harbor::new(&eval).unwrap();
    let result: JobResult =
        serde_json::from_slice(&fs::read(eval.directory().join("result.json")).unwrap()).unwrap();
    assert_eq!(result.n_total_trials, 2);
    assert_eq!(result.stats.completed, 0);
    assert_eq!(result.stats.pending, 2);
}

#[test]
fn job_config_records_microvm_backend_before_execution() {
    let output = tempdir().unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .attempt_environment(EvalEnvironment::MicroVm)
        .build()
        .unwrap();

    Harbor::new(&eval).unwrap();
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(eval.directory().join("config.json")).unwrap()).unwrap();
    assert_eq!(
        config["environment"]["import_path"],
        "nanocodex_vm:VmEnvironment"
    );
    assert_eq!(config["environment"]["kwargs"]["backend"], "microvm");
}

#[tokio::test]
async fn records_an_errored_attempt_as_a_harbor_trial() {
    let task_root = tempdir().unwrap();
    fs::create_dir(task_root.path().join("tests")).unwrap();
    fs::create_dir(task_root.path().join("environment")).unwrap();
    fs::write(
        task_root.path().join("task.toml"),
        r#"
schema_version = "1.1"
[task]
name = "terminal-bench/errored"
description = "Errored Harbor fixture"
[metadata]
custom_docker_compose = true
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "example/errored:latest"
cpus = 1
memory_mb = 128
storage_mb = 128
gpus = 0
allow_internet = false
"#,
    )
    .unwrap();
    fs::write(task_root.path().join("instruction.md"), "do the work\n").unwrap();
    fs::write(task_root.path().join("tests/test.sh"), "exit 0\n").unwrap();
    let task = Task::load(task_root.path()).unwrap();
    let output = tempdir().unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test").unwrap()))
        .output_directory(output.path())
        .build()
        .unwrap();
    let run = eval.task(task);
    let recorder = Harbor::new(&eval)
        .unwrap()
        .record(run.events().subscribe())
        .unwrap();

    assert!(run.await.unwrap().unscored().is_some_and(|failure| {
        failure.exception.kind == crate::EvalExceptionKind::Environment
    }));
    let job = recorder.finish_all(1).await.unwrap();
    let trial = fs::read_dir(job.directory())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .unwrap()
        .path();
    let result: TrialResult =
        serde_json::from_slice(&fs::read(trial.join("result.json")).unwrap()).unwrap();
    let exception = result.exception_info.unwrap();
    assert_eq!(exception.exception_type, "EnvironmentError");
    assert!(
        exception
            .exception_message
            .contains("custom Docker Compose")
    );
    serde_json::from_slice::<AtifTrajectory>(
        &fs::read(trial.join("agent/trajectory.json")).unwrap(),
    )
    .unwrap();

    let result: JobResult =
        serde_json::from_slice(&fs::read(job.directory().join("result.json")).unwrap()).unwrap();
    assert_eq!(result.n_total_trials, 1);
    assert_eq!(result.stats.completed, 1);
    assert_eq!(result.stats.errored, 1);
    assert_eq!(result.stats.pending, 0);
}

fn test_sweep(task: Task, trials: u16) -> Sweep {
    Sweep::builder()
        .task(task)
        .trials(trials)
        .agent(
            "default",
            Nanocodex::builder(OpenAi::new("test-key").unwrap()),
        )
        .unwrap()
        .build()
        .unwrap()
}

fn test_recorder(eval: &Evaluator) -> (broadcast::Sender<Arc<EvalEvent>>, HarborRecorder) {
    let (sender, _) = broadcast::channel(8);
    let events = EvalEvents::new(&sender);
    let recorder = Harbor::new(eval)
        .unwrap()
        .record(events.subscribe())
        .unwrap();
    (sender, recorder)
}

fn finite_trial_name(task: &Task, agent: &str, trial: u16, attempt_id: Uuid) -> String {
    let short_name = task.name().rsplit('/').next().unwrap();
    format!("{short_name}__{agent}__{trial:03}__{}", attempt_id.simple())
}

fn started_event(
    eval: &Evaluator,
    task: &Task,
    attempt_id: Uuid,
    trial_name: &str,
) -> Arc<EvalEvent> {
    Arc::new(EvalEvent {
        run_id: eval.id(),
        invocation_id: Uuid::nil(),
        sequence: 1,
        attempt: Some(EvalEventAttempt {
            id: attempt_id,
            task_name: task.name().to_owned(),
            trial_name: trial_name.to_owned(),
            sequence: 1,
        }),
        kind: EvalEventKind::AttemptStarted {
            prompt: task.prompt().to_owned(),
            workspace: eval.directory().join(trial_name).join("workspace"),
        },
    })
}

fn failed_event(
    eval: &Evaluator,
    task: Task,
    attempt_id: Uuid,
    trial_name: String,
    sequence: u64,
) -> Arc<EvalEvent> {
    let occurred_at = Utc::now();
    let root = eval.directory().join(&trial_name);
    Arc::new(EvalEvent {
        run_id: eval.id(),
        invocation_id: Uuid::nil(),
        sequence,
        attempt: Some(EvalEventAttempt {
            id: attempt_id,
            task_name: task.name().to_owned(),
            trial_name: trial_name.clone(),
            sequence,
        }),
        kind: EvalEventKind::Failed(Box::new(EvalFailure {
            attempt_id,
            task_name: task.name().to_owned(),
            trial_name,
            exception: EvalException {
                kind: EvalExceptionKind::Environment,
                outcome: EvalOutcome::InfrastructureError,
                message: "deterministic test failure".to_owned(),
                traceback: "deterministic test failure".to_owned(),
                occurred_at,
            },
            model: "gpt-test".to_owned(),
            effort: "high".to_owned(),
            environment: EvalEnvironment::Native,
            started_at: occurred_at,
            finished_at: occurred_at,
            timing: EvalFailureTiming {
                queue_wait: PhaseTiming {
                    started_at: occurred_at,
                    finished_at: occurred_at,
                },
                environment_setup: None,
                environment_readiness: None,
                agent_setup: None,
                agent_execution: None,
                verifier: None,
            },
            agent: None,
            verifier: None,
            cleanup: EvalCleanup::default(),
            artifacts: EvalArtifacts {
                directory: root.clone(),
                workspace: root.join("workspace"),
                verifier_output: root.join("verifier/test-stdout.txt"),
            },
            task,
        })),
    })
}

async fn wait_for_job_state(directory: &Path, expected: (usize, usize, usize, usize)) -> JobResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = read_job(directory);
        if job_state(&result) == expected {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "job state remained {:?}, expected {expected:?}",
            job_state(&result)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_recorder_stop(recorder: &HarborRecorder) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !recorder
        .task
        .as_ref()
        .is_some_and(tokio::task::JoinHandle::is_finished)
    {
        assert!(Instant::now() < deadline, "Harbor recorder did not stop");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[derive(Debug, Eq, PartialEq)]
struct FileSnapshot {
    inode: u64,
    bytes: Vec<u8>,
}

fn file_snapshot(path: &Path) -> FileSnapshot {
    FileSnapshot {
        inode: fs::metadata(path).unwrap().ino(),
        bytes: fs::read(path).unwrap(),
    }
}

fn read_job(directory: &Path) -> JobResult {
    serde_json::from_slice(&fs::read(directory.join("result.json")).unwrap()).unwrap()
}

fn assert_job_state(result: &JobResult, expected: (usize, usize, usize, usize)) {
    assert_eq!(job_state(result), expected);
}

const fn job_state(result: &JobResult) -> (usize, usize, usize, usize) {
    (
        result.n_total_trials,
        result.stats.completed,
        result.stats.running,
        result.stats.pending,
    )
}

fn write_greeting_task() -> Task {
    Task::load(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
    )
    .unwrap()
}

fn write_retained_trial(
    job: &Path,
    job_id: Uuid,
    task: &Task,
    configuration: &str,
    repetition: u16,
    reward: Option<f64>,
    cleanup_failed: bool,
) -> Uuid {
    let id = Uuid::now_v7();
    let compact_id = id.simple().to_string();
    let trial_name = format!(
        "write-greeting__{configuration}__{repetition:03}__{}",
        &compact_id[..8]
    );
    let directory = job.join(&trial_name);
    fs::create_dir_all(directory.join("agent")).unwrap();
    fs::create_dir_all(directory.join("verifier")).unwrap();
    fs::write(directory.join("agent/trajectory.json"), "{}\n").unwrap();
    fs::write(directory.join("verifier/test-stdout.txt"), "fixture\n").unwrap();

    let started_at = Utc::now();
    let finished_at = started_at + chrono::Duration::milliseconds(10);
    let phase = json!({
        "started_at": started_at,
        "finished_at": finished_at,
    });
    let agent_result = reward.map(|_| {
        json!({
            "n_input_tokens": 10,
            "n_cache_tokens": 4,
            "n_output_tokens": 3,
            "cost_usd": 0.25,
            "billing_completeness": "complete",
            "metadata": {
                "status": "completed",
                "model": "gpt-test",
                "effort": "high",
                "reasoning_mode": "adaptive",
                "transport": "responses_websocket_v2",
                "orchestration": "local_code_mode",
                "runtime_completeness": "complete",
                "duration_ms": 10,
                "duration_ns": 10_000_000,
                "model_calls": 1,
                "steers": 0,
                "compactions": 0,
                "tool_calls": 1,
                "connection_attempts": 1,
                "websocket_reconnects": 0,
                "response_attempts": 1,
                "response_retries": 0,
                "billing_uncertain_response_attempts": 0,
                "connection_duration_ns": 1,
                "retry_backoff_duration_ns": 0,
                "model_duration_ns": 5,
                "warmup_duration_ns": 0,
                "tool_work_duration_ns": 6,
                "tool_wall_duration_ns": 7,
                "usage": {
                    "input_tokens": 10,
                    "cached_input_tokens": 4,
                    "cache_write_input_tokens": 2,
                    "output_tokens": 3,
                    "reasoning_output_tokens": 1,
                    "total_tokens": 13,
                },
                "warmup_usage": {
                    "input_tokens": 2,
                    "cached_input_tokens": 2,
                    "cache_write_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 2,
                },
                "estimated_cost": {
                    "usd": "0.25",
                    "input_usd": "0.1",
                    "cached_input_usd": "0.02",
                    "cache_write_input_usd": "0.03",
                    "output_usd": "0.1",
                    "service_tier": "standard",
                },
                "cost_usd": 0.25,
                "cost_status": "estimated_from_usage",
            },
        })
    });
    let verifier_result = reward.map(|reward| {
        json!({
            "exit_code": 0,
            "rewards": {
                "reward": reward,
            },
        })
    });
    let exception_info = reward.is_none().then(|| {
        json!({
            "exception_type": "AgentError",
        })
    });
    let timing = reward.map(|_| phase.clone());
    let mut result = json!({
        "id": id,
        "task_name": "nanoeval/write-greeting",
        "trial_name": trial_name,
        "task_checksum": "fixture-checksum",
        "task_id": {
            "path": task.root(),
        },
        "source": "nanocodex/local",
        "outcome": if reward.is_some_and(|reward| reward > 0.0) {
            "passed"
        } else if reward.is_some() {
            "verifier_failed"
        } else {
            "infrastructure_error"
        },
        "scored": reward.is_some(),
        "cleanup": EvalCleanup::default(),
        "config": {
            "task": {
                "path": task.root(),
            },
            "trial_name": trial_name,
            "trials_dir": job,
            "job_id": job_id,
            "environment": {
                "kwargs": {
                    "backend": "native",
                },
            },
        },
        "agent_info": {
            "name": "nanocodex",
            "version": "test",
            "model_info": {
                "name": "gpt-test",
            },
        },
        "agent_result": agent_result,
        "verifier_result": verifier_result,
        "started_at": started_at,
        "finished_at": finished_at,
        "queue_wait": timing,
        "environment_readiness": timing,
        "agent_setup": timing,
        "agent_execution": timing,
        "verifier": timing,
        "exception_info": exception_info,
    });
    if cleanup_failed {
        result["outcome"] = json!("passed");
        result["scored"] = json!(true);
        result["cleanup"] = json!({
            "agent": {
                "status": "completed",
                "timing": phase,
                "diagnostic": null,
            },
            "verifier": {
                "status": "failed",
                "timing": phase,
                "diagnostic": {
                    "message": "deterministic cleanup failure",
                    "traceback": "deterministic cleanup failure",
                },
            },
        });
        result["exception_info"] = serde_json::Value::Null;
    }
    super::HarborArtifacts::write_json(
        &directory.join("lock.json"),
        &super::HarborTrialLock::new(
            task,
            "gpt-test",
            "high",
            &super::packager_content_hash(task.root()).unwrap(),
            task.content_digest(),
            EvalEnvironment::Native,
        ),
    )
    .unwrap();
    fs::write(
        directory.join("result.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    id
}
