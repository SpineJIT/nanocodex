use std::{
    collections::HashMap,
    fs,
    sync::{Arc, Mutex, Once, OnceLock},
    time::Duration,
};

use nanocodex_agent::{Nanocodex, NanocodexError, OpenAi, transport::ResponsesError};
use tempfile::tempdir;
use tracing::{Id, Instrument, Subscriber, field::Visit, span::Attributes};
use tracing_subscriber::{Layer, layer::Context as LayerContext, prelude::*, registry::LookupSpan};
use uuid::Uuid;

use super::{
    AdmissionController, EvalError, Evaluator, SweepCoordinate, failure_kind, failure_outcome,
    output_aliases_task_package, trial_name, validate_attempt_environment,
};
use crate::{
    EvalEventKind, EvalExceptionKind, EvalOutcome, Sweep, Task, native::NativeAttempt,
    sweep::AgentId,
};

#[derive(Clone, Default)]
struct TraceCapture(Arc<Mutex<HashMap<u64, CapturedSpan>>>);
static TRACE_CAPTURE: OnceLock<TraceCapture> = OnceLock::new();
static TRACE_SUBSCRIBER: Once = Once::new();

struct CapturedSpan {
    name: &'static str,
    parent: Option<u64>,
    fields: HashMap<String, String>,
}

struct FieldCapture<'a>(&'a mut HashMap<String, String>);

fn install_trace_capture() -> TraceCapture {
    let capture = TRACE_CAPTURE.get_or_init(TraceCapture::default).clone();
    TRACE_SUBSCRIBER.call_once(|| {
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(capture.clone()),
        )
        .expect("test process has no pre-existing global tracing subscriber");
        tracing::callsite::rebuild_interest_cache();
    });
    capture
}

#[test]
fn fresh_finite_run_is_bound_before_execution() {
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

    assert!(!eval.resumed());
    assert_eq!(eval.planned_attempts(), Some(2));
    assert_eq!(eval.remaining_attempts().unwrap(), 2);
    assert!(eval.directory().join("run.json").is_file());
}

#[tokio::test]
async fn invocation_events_end_with_one_terminal_and_close() {
    let output = tempdir().unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .build()
        .unwrap();
    let run = eval.tasks(Vec::new());
    let invocation_id = run.id();
    let mut events = run.events().subscribe();

    assert!(matches!(run.await, Err(EvalError::NoTasks)));
    let terminal = events.recv().await.unwrap().unwrap();
    assert_eq!(terminal.invocation_id, invocation_id);
    assert!(matches!(terminal.kind, EvalEventKind::RunFailed { .. }));
    assert!(events.recv().await.unwrap().is_none());
}

#[tokio::test]
async fn dropping_an_unpolled_invocation_emits_cancellation_and_closes() {
    let output = tempdir().unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(output.path())
        .build()
        .unwrap();
    let run = eval.tasks(Vec::new());
    let mut events = run.events().subscribe();

    drop(run);

    let terminal = events.recv().await.unwrap().unwrap();
    assert!(matches!(terminal.kind, EvalEventKind::RunFailed { .. }));
    assert!(events.recv().await.unwrap().is_none());
}

#[test]
fn colliding_task_names_and_immediate_retries_get_distinct_attempt_paths() {
    let tasks = tempdir().unwrap();
    let first = write_named_task(tasks.path(), "first", "one/shared");
    let second = write_named_task(tasks.path(), "second", "two/shared");
    let coordinate = SweepCoordinate {
        agent: AgentId::new("default").unwrap(),
        trial: 1,
    };
    let ids = [
        Uuid::from_u128(0x1234_5678_0000_0000_0000_0000_0000_0001),
        Uuid::from_u128(0x1234_5678_0000_0000_0000_0000_0000_0002),
        Uuid::from_u128(0x1234_5678_0000_0000_0000_0000_0000_0003),
    ];
    let first_name = trial_name(&first, ids[0], Some(&coordinate));
    let second_name = trial_name(&second, ids[1], Some(&coordinate));
    let retry_name = trial_name(&first, ids[2], Some(&coordinate));

    let compact_ids = ids.map(|id| id.simple().to_string());
    assert_eq!(&compact_ids[0][..8], &compact_ids[1][..8]);
    assert_eq!(&compact_ids[0][..8], &compact_ids[2][..8]);
    assert_ne!(first_name, second_name);
    assert_ne!(first_name, retry_name);
    assert_ne!(second_name, retry_name);
    assert!(first_name.ends_with(&compact_ids[0]));
    assert!(second_name.ends_with(&compact_ids[1]));
    assert!(retry_name.ends_with(&compact_ids[2]));

    let output = tempdir().unwrap();
    let first_attempt = NativeAttempt::prepare(output.path(), &first_name, &first).unwrap();
    fs::write(
        first_attempt.paths.workspace.join("abandoned-partial"),
        "partial\n",
    )
    .unwrap();
    let second_attempt = NativeAttempt::prepare(output.path(), &second_name, &second).unwrap();
    let retry_attempt = NativeAttempt::prepare(output.path(), &retry_name, &first).unwrap();

    assert_ne!(first_attempt.paths.root, second_attempt.paths.root);
    assert_ne!(first_attempt.paths.root, retry_attempt.paths.root);
    assert!(
        !retry_attempt
            .paths
            .workspace
            .join("abandoned-partial")
            .exists()
    );
}

#[test]
fn admission_is_work_conserving_within_memory_and_concurrency_limits() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let admission = Arc::new(AdmissionController::new(2, Some(4)));
        let three = admission.acquire(3).await.unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(5), admission.acquire(2))
                .await
                .is_err()
        );
        let one = tokio::time::timeout(Duration::from_millis(5), admission.acquire(1))
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), admission.acquire(1))
                .await
                .is_err()
        );

        drop(one);
        drop(three);
        let oversized = admission.acquire(10).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), admission.acquire(1))
                .await
                .is_err()
        );
        drop(oversized);
    });
}

#[test]
fn admission_release_before_wait_registration_is_not_lost() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let admission = Arc::new(AdmissionController::new(1, None));
        let permit = admission.acquire(1).await.unwrap();
        let generation = admission.capacity_generation();

        drop(permit);

        tokio::time::timeout(
            Duration::from_millis(5),
            admission.wait_for_change(generation),
        )
        .await
        .expect("a release before listener registration must still be visible");
    });
}

#[test]
fn draining_closes_admission_without_cancelling_admitted_work() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let admission = Arc::new(AdmissionController::new(1, None));
        let admitted = admission.acquire(1).await.unwrap();
        let waiting = {
            let admission = Arc::clone(&admission);
            tokio::spawn(async move { admission.acquire(1).await })
        };

        assert_eq!(admission.begin_drain(), 1);
        assert_eq!(admission.begin_drain(), 1);
        assert!(waiting.await.unwrap().is_none());

        drop(admitted);
        assert!(admission.acquire(1).await.is_none());
    });
}

impl Visit for FieldCapture<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl<S> Layer<S> for TraceCapture
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: LayerContext<'_, S>) {
        let parent = attributes
            .parent()
            .map(|parent| parent.clone().into_u64())
            .or_else(|| {
                attributes
                    .is_contextual()
                    .then(|| context.current_span().id().map(Id::into_u64))
                    .flatten()
            });
        let mut fields = HashMap::new();
        attributes.record(&mut FieldCapture(&mut fields));
        self.0.lock().unwrap().insert(
            id.clone().into_u64(),
            CapturedSpan {
                name: attributes.metadata().name(),
                parent,
                fields,
            },
        );
    }

    fn on_record(
        &self,
        id: &Id,
        values: &tracing::span::Record<'_>,
        _context: LayerContext<'_, S>,
    ) {
        if let Some(span) = self.0.lock().unwrap().get_mut(&id.clone().into_u64()) {
            values.record(&mut FieldCapture(&mut span.fields));
        }
    }
}

#[test]
fn failed_attempt_does_not_cancel_pending_batch_work() {
    let capture = install_trace_capture();
    let task_root = tempdir().unwrap();
    fs::create_dir(task_root.path().join("tests")).unwrap();
    fs::create_dir(task_root.path().join("environment")).unwrap();
    fs::write(
        task_root.path().join("task.toml"),
        r#"
schema_version = "1.1"
[task]
name = "terminal-bench/traced"
description = "Tracing fixture"
[metadata]
custom_docker_compose = true
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "example/traced:latest"
cpus = 1
memory_mb = 128
storage_mb = 128
gpus = 0
allow_internet = false
"#,
    )
    .unwrap();
    fs::write(
        task_root.path().join("instruction.md"),
        "do the traced work\n",
    )
    .unwrap();
    fs::write(task_root.path().join("tests/test.sh"), "exit 0\n").unwrap();
    let task = Task::load(task_root.path()).unwrap();
    assert!(matches!(
        validate_attempt_environment(&task, false),
        Err(EvalError::UnsupportedNativeTask { .. })
    ));
    assert!(validate_attempt_environment(&task, true).is_ok());
    let output = tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let eval_id = runtime.block_on(async {
        let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test").unwrap()))
            .output_directory(output.path())
            .build()
            .unwrap();
        let eval_id = eval.id().to_string();
        let result = eval
            .tasks(vec![task.clone(), task])
            .instrument(tracing::info_span!("test.parent"))
            .await;
        let outcomes = result.expect("accepted failures must remain in the batch result");
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|outcome| {
            outcome
                .unscored()
                .is_some_and(|failure| failure.exception.kind == EvalExceptionKind::Environment)
        }));
        eval_id
    });

    let spans = capture.0.lock().unwrap();
    let attempts = spans
        .iter()
        .filter(|(_, span)| {
            span.name == "eval.attempt"
                && span.fields.get("eval.id").is_some_and(|id| id == &eval_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 2);
    for (_, attempt) in &attempts {
        assert!(attempt.parent.is_none());
        assert_eq!(
            attempt.fields.get("status").map(String::as_str),
            Some("failed")
        );
        assert!(attempt.fields.contains_key("duration_ns"));
    }
    let setups = spans
        .values()
        .filter(|span| {
            span.name == "eval.environment.setup"
                && attempts
                    .iter()
                    .any(|(attempt_id, _)| span.parent == Some(**attempt_id))
        })
        .collect::<Vec<_>>();
    assert_eq!(setups.len(), 2);
    for setup in setups {
        assert!(
            attempts
                .iter()
                .any(|(attempt_id, _)| setup.parent == Some(**attempt_id))
        );
        assert_eq!(
            setup.fields.get("status").map(String::as_str),
            Some("failed")
        );
        assert!(setup.fields.contains_key("duration_ns"));
    }
}

#[test]
fn classifies_cyber_policy_as_an_agent_safety_refusal() {
    let error = EvalError::Nanocodex(NanocodexError::Response(
        ResponsesError::Api {
            event: r#"{"type":"error","error":{"code":"cyber_policy"}}"#.to_owned(),
        }
        .into(),
    ));

    assert_eq!(failure_kind(&error), EvalExceptionKind::AgentSafetyRefusal);
}

#[test]
fn classifies_stock_codex_policy_rejection_as_an_agent_safety_refusal() {
    let error = EvalError::Codex(crate::CodexExecError::SafetyRefusal(
        "flagged for possible cybersecurity risk".to_owned(),
    ));

    assert_eq!(failure_outcome(&error), EvalOutcome::SafetyRefusal);
    assert_eq!(failure_kind(&error), EvalExceptionKind::AgentSafetyRefusal);
}

#[test]
fn rejects_a_task_package_mutated_after_load_before_attempt_setup() {
    let tasks = tempdir().unwrap();
    let task = write_named_task(tasks.path(), "changed", "terminal-bench/changed");
    fs::write(task.environment_directory().join("late-input"), "changed\n").unwrap();
    let output = tempdir().unwrap();
    let eval = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test").unwrap()))
        .output_directory(output.path())
        .build()
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let outcome = runtime.block_on(eval.task(task)).unwrap();
    let failure = outcome
        .unscored()
        .expect("task package mutation must be an unscored attempt");

    assert!(matches!(
        failure.exception.kind,
        EvalExceptionKind::Environment
    ));
    assert!(
        fs::read_dir(eval.directory())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_type().is_ok_and(|kind| kind.is_dir()))
    );
}

#[test]
fn rejects_finite_output_nested_in_a_hashed_task_package_before_creation() {
    let tasks = tempdir().unwrap();
    let task = write_named_task(tasks.path(), "overlap", "terminal-bench/overlap");
    let output = task.environment_directory().join("retained/evals");
    let sweep = Sweep::builder()
        .task(task)
        .agent(
            "default",
            Nanocodex::builder(OpenAi::new("test-key").unwrap()),
        )
        .unwrap()
        .build()
        .unwrap();

    let result = Evaluator::new_builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
        .output_directory(&output)
        .fresh_run(sweep)
        .build();

    assert!(matches!(result, Err(EvalError::OutputOverlapsTask { .. })));
    assert!(!output.exists());
}

#[cfg(unix)]
#[test]
fn detects_an_output_directory_reached_through_a_filesystem_alias() {
    let tasks = tempdir().unwrap();
    let task = write_named_task(tasks.path(), "alias", "terminal-bench/alias");
    let aliases = tempdir().unwrap();
    let alias = aliases.path().join("environment");
    std::os::unix::fs::symlink(task.environment_directory(), &alias).unwrap();

    assert!(output_aliases_task_package(&alias.join("retained/evals"), task.root()).unwrap());
}

fn write_named_task(parent: &std::path::Path, directory: &str, name: &str) -> Task {
    let root = parent.join(directory);
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("environment")).unwrap();
    fs::create_dir(root.join("tests")).unwrap();
    fs::write(root.join("instruction.md"), "Do the work.\n").unwrap();
    fs::write(root.join("tests/test.sh"), "exit 0\n").unwrap();
    fs::write(
        root.join("task.toml"),
        format!(
            r#"
schema_version = "1.1"
[task]
name = "{name}"
description = "attempt identity fixture"
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "alpine:3.21"
cpus = 1
memory_mb = 128
storage_mb = 128
gpus = 0
allow_internet = false
"#
        ),
    )
    .unwrap();
    Task::load(root).unwrap()
}
