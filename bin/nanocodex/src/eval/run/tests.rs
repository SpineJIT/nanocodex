use std::{
    cell::Cell,
    fs, future,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Parser;
use nanocodex::{Nanocodex, OpenAi, Thinking};
use nanocodex_eval::{BillingCompleteness, Evaluator, Sweep, Task, vm::VmBackend};
use sha2::Digest as _;

use super::{
    EvalInterruptError, HostResources, InterruptListener, RetainedBuild, RetainedScheduling, Run,
    RunInvocation, RunMeasurements, RunSummary, VmRetention, desired_eval_open_file_limit,
    finish_or_drain, finish_or_interrupt, load_tasks, retained_retry_task_names,
    retained_task_durations,
};
use crate::eval::args::{DEFAULT_HOST_UTILIZATION_PERCENT, DEFAULT_TRIALS};

#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    eval: Run,
}

#[tokio::test]
async fn injected_interrupt_closes_admission_then_waits_for_admitted_work() {
    let (release, released) = tokio::sync::oneshot::channel();
    let (signal, interrupts) = injected_interrupts();
    signal.send(Ok(())).unwrap();
    let execution = finish_or_drain(
        async {
            released.await.unwrap();
            Ok::<_, &'static str>(17)
        },
        interrupts,
        9,
        || {
            release.send(()).unwrap();
            3
        },
    )
    .await
    .unwrap();

    assert_eq!(execution.result.unwrap(), 17);
    assert_eq!(execution.terminal_attempts, 3);
    assert!(execution.interrupted);
}

#[tokio::test]
async fn immediate_second_interrupt_is_not_lost_and_drops_draining_work() {
    let (started_sender, started) = tokio::sync::oneshot::channel();
    let (dropped_sender, dropped) = tokio::sync::oneshot::channel();
    let (signals, interrupts) = injected_interrupts();
    let send_signals = tokio::spawn(async move {
        started.await.unwrap();
        signals.send(Ok(())).unwrap();
        signals.send(Ok(())).unwrap();
    });
    let drain_count = Cell::new(0);
    let result = finish_or_drain(
        async move {
            let _drop_signal = DropSignal(Some(dropped_sender));
            started_sender.send(()).unwrap();
            future::pending::<Result<(), &'static str>>().await
        },
        interrupts,
        9,
        || {
            drain_count.set(drain_count.get() + 1);
            3
        },
    )
    .await;

    assert!(matches!(result, Err(EvalInterruptError::Forced)));
    assert_eq!(drain_count.get(), 1);
    dropped.await.unwrap();
    send_signals.await.unwrap();
}

#[tokio::test]
async fn pending_interrupt_listener_remains_actionable_during_finalization() {
    let (interrupt_sender, interrupts) = injected_interrupts();
    let execution = finish_or_drain(
        future::ready(Ok::<_, &'static str>(17)),
        interrupts,
        1,
        || unreachable!(),
    )
    .await
    .unwrap();
    let (dropped_sender, dropped) = tokio::sync::oneshot::channel();
    let result = finish_or_interrupt(
        async move {
            let _drop_signal = DropSignal(Some(dropped_sender));
            interrupt_sender.send(Ok(())).unwrap();
            future::pending::<()>().await
        },
        execution.interrupt,
    )
    .await;

    assert!(matches!(result, Err(EvalInterruptError::Finalization)));
    dropped.await.unwrap();
}

fn injected_interrupts() -> (
    tokio::sync::mpsc::UnboundedSender<std::io::Result<()>>,
    InterruptListener,
) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    (sender, InterruptListener::Injected(receiver))
}

struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[test]
fn run_measurements_retain_cold_and_warm_phase_boundaries() {
    let output = tempfile::tempdir().unwrap();
    RunMeasurements {
        observability: Duration::from_nanos(1),
        task_loading: Duration::from_nanos(2),
        vm_runtime: Duration::from_nanos(3),
        vm_environments: Duration::from_nanos(4),
        evaluation_setup: Duration::from_nanos(5),
        attempts: Duration::from_nanos(6),
        harbor_finish: Duration::from_nanos(7),
        output: Duration::from_nanos(8),
        total: Duration::from_nanos(36),
    }
    .persist(output.path())
    .unwrap();

    let timing: serde_json::Value =
        serde_json::from_slice(&fs::read(output.path().join("timing.json")).unwrap()).unwrap();
    assert_eq!(timing["vm_runtime_build_ns"], 3);
    assert_eq!(timing["cold_image_and_cache_ns"], 4);
    assert_eq!(timing["attempts_wall_ns"], 6);
    assert_eq!(timing["total_wall_ns"], 36);
}

#[test]
fn vm_guest_build_targets_the_unified_vm_package() {
    let command = super::vm_guest_build_command(Path::new("/tmp/nanocodex-workspace"));
    let arguments = command
        .as_std()
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        arguments,
        [
            "build",
            "--quiet",
            "--locked",
            "--target",
            super::VM_GUEST_TARGET,
            "--package",
            "nanocodex-vm",
            "--bin",
            "nanocodex-vm-guest",
            "--no-default-features",
            "--features",
            "guest-runtime",
        ]
    );
    assert_eq!(
        command.as_std().get_current_dir(),
        Some(Path::new("/tmp/nanocodex-workspace"))
    );
}

#[test]
fn guest_build_record_tracks_exact_cargo_dependencies() {
    let workspace = tempfile::tempdir().unwrap();
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config.toml",
        "crates/nanocodex-oai-api/Cargo.toml",
        "crates/nanocodex-tools/Cargo.toml",
        "crates/experimental/nanocodex-vm/Cargo.toml",
    ] {
        let path = workspace.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "fixture").unwrap();
    }
    let source = workspace
        .path()
        .join("crates/experimental/nanocodex-vm/src/tools/guest.rs");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "first guest source").unwrap();
    let runtime = workspace.path().join("target/guest/debug/guest");
    fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    fs::write(&runtime, "guest binary").unwrap();
    fs::write(
        runtime.with_extension("d"),
        format!("{}: {}\n", runtime.display(), source.display()),
    )
    .unwrap();

    super::write_vm_guest_build_record(workspace.path(), &runtime).unwrap();
    assert!(super::vm_guest_runtime_is_fresh(workspace.path(), &runtime).unwrap());

    fs::write(source, "changed guest source with a different size").unwrap();
    assert!(!super::vm_guest_runtime_is_fresh(workspace.path(), &runtime).unwrap());
}

#[test]
fn cargo_dep_info_parser_preserves_escaped_paths() {
    let paths = super::parse_cargo_dep_info(
        "/tmp/guest: /tmp/plain.rs /tmp/with\\ space.rs /tmp/back\\\\slash.rs\n",
    )
    .unwrap();

    assert_eq!(
        paths,
        [
            PathBuf::from("/tmp/plain.rs"),
            PathBuf::from("/tmp/with space.rs"),
            PathBuf::from("/tmp/back\\slash.rs"),
        ]
    );
}

#[test]
fn accepts_repeated_tasks_with_per_task_trials() {
    let cli = TestCli::try_parse_from([
        "nanoeval",
        "--task",
        "tasks/first",
        "--task",
        "tasks/second",
        "--trials",
        "5",
        "--concurrency",
        "10",
        "--max-memory-mb",
        "24576",
    ])
    .unwrap();

    assert_eq!(
        cli.eval.tasks,
        [PathBuf::from("tasks/first"), PathBuf::from("tasks/second")]
    );
    assert_eq!(cli.eval.scheduling.trials, 5);
    assert_eq!(cli.eval.scheduling.concurrency, Some(10));
    assert_eq!(cli.eval.scheduling.max_memory_mb, Some(24_576));
    assert_eq!(
        cli.eval.scheduling.host_utilization,
        DEFAULT_HOST_UTILIZATION_PERCENT
    );
    assert!(!cli.eval.vm_retention.unwrap_or_default().retains_passes());
    assert!(cli.eval.suites.is_empty());
}

#[test]
fn defaults_to_five_independent_trials_per_task() {
    let cli = TestCli::try_parse_from(["nanoeval", "--task", "tasks/first"]).unwrap();

    let resolved = cli.eval.resolve_run().unwrap();

    assert_eq!(cli.eval.scheduling.trials, DEFAULT_TRIALS);
    assert_eq!(resolved.trials, DEFAULT_TRIALS);
    assert!(!resolved.web_search);
}

#[test]
fn web_search_is_an_explicit_eval_capability() {
    let cli =
        TestCli::try_parse_from(["nanoeval", "--task", "tasks/first", "--web-search", "true"])
            .unwrap();

    let resolved = cli.eval.resolve_run().unwrap();

    assert!(resolved.web_search);
}

#[test]
fn prebuilt_guest_runtime_is_an_explicit_eval_artifact() {
    let cli = TestCli::try_parse_from([
        "nanoeval",
        "--task",
        "tasks/first",
        "--vm-guest-runtime",
        "/opt/nanocodex-vm-guest",
    ])
    .unwrap();

    let resolved = cli.eval.resolve_run().unwrap();

    assert_eq!(
        resolved.vm_guest_runtime,
        Some(PathBuf::from("/opt/nanocodex-vm-guest"))
    );
}

#[test]
fn shared_vm_cache_is_an_explicit_eval_resource() {
    let cli = TestCli::try_parse_from([
        "nanoeval",
        "--task",
        "tasks/first",
        "--vm-cache",
        "/var/cache/nanocodex-vm",
    ])
    .unwrap();

    let resolved = cli.eval.resolve_run().unwrap();

    assert_eq!(resolved.vm_cache, PathBuf::from("/var/cache/nanocodex-vm"));
}

#[tokio::test]
async fn explicit_guest_runtime_rejects_the_wrong_elf_machine() {
    let job = tempfile::tempdir().unwrap();
    let runtime = job.path().join("wrong-architecture");
    let wrong_machine = if super::VM_GUEST_ELF_MACHINE == 62 {
        183
    } else {
        62
    };
    fs::write(&runtime, guest_elf(wrong_machine)).unwrap();

    let error = super::prepare_runtime_for_vm(None, Some(&runtime), job.path(), None)
        .await
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains(&format!("e_machine {wrong_machine}"))
    );
    assert!(error.to_string().contains(super::VM_GUEST_TARGET));
    assert!(!job.path().join(super::GUEST_RUNTIME_ARTIFACT_ROOT).exists());
}

#[tokio::test]
async fn implicit_resume_rebuilds_from_the_job_owned_guest_artifact() {
    let root = tempfile::tempdir().unwrap();
    let task = write_test_task(&root.path().join("task"));
    let output = root.path().join("jobs");
    let agent = Nanocodex::builder(OpenAi::new("test").unwrap());
    let sweep = Sweep::builder()
        .tasks(vec![task.clone()])
        .trials(1)
        .agent("default", agent.clone())
        .unwrap()
        .build()
        .unwrap();
    let first = Evaluator::builder(agent.clone(), VmBackend::builder().build())
        .output_directory(&output)
        .resume_incomplete(sweep.clone())
        .build()
        .unwrap();
    assert!(!first.resumed());

    let source = root.path().join("mutable-workspace-guest");
    fs::write(&source, guest_elf(super::VM_GUEST_ELF_MACHINE)).unwrap();
    let (_, first_disk, first_runtime, _) = super::prepare_run_vm(
        None,
        Some(&source),
        first.directory(),
        first.resumed(),
        None,
        false,
    )
    .await
    .unwrap();
    let first_runtime = first_runtime.unwrap();
    let artifact = first
        .directory()
        .join(first_runtime.artifact_path.as_ref().unwrap());
    assert!(artifact.is_file());
    assert!(first_disk.starts_with(first.directory()));

    let resolved = super::ResolvedRun {
        task_paths: vec![task.root().to_path_buf()],
        output: output.clone(),
        trials: 1,
        concurrency: 1,
        max_memory_mb: None,
        vm_rootfs: None,
        vm_guest_runtime: Some(source.clone()),
        vm_cache: PathBuf::from(".cache/vm"),
        vm_retention: VmRetention::Failures,
        thinking: Thinking::Low,
        web_search: false,
        rerun_from: None,
        automatic_scheduling: None,
    };
    super::persist_invocation(
        first.directory(),
        &resolved.invocation(Some(first_runtime.clone())).unwrap(),
    )
    .unwrap();
    let job = first.directory().to_path_buf();
    drop(first);

    fs::write(&source, b"overwritten mutable build output").unwrap();
    fs::remove_dir_all(job.join(super::GUEST_RUNTIME_CACHE_ROOT)).unwrap();

    let resumed = Evaluator::builder(agent, VmBackend::builder().build())
        .output_directory(&output)
        .resume_incomplete(sweep)
        .build()
        .unwrap();
    assert!(resumed.resumed());
    assert_eq!(resumed.directory(), job);
    let (_, resumed_disk, resumed_runtime, _) = super::prepare_run_vm(
        None,
        None,
        resumed.directory(),
        resumed.resumed(),
        None,
        true,
    )
    .await
    .unwrap();

    assert!(resumed_disk.is_file());
    assert!(resumed_disk.starts_with(resumed.directory()));
    assert_eq!(resumed_runtime.unwrap(), first_runtime);
    drop(resumed);
}

#[test]
fn retained_resume_rehydrates_missing_job_runtime_from_exact_requested_elf() {
    let root = tempfile::tempdir().unwrap();
    let job = root.path().join("job");
    fs::create_dir(&job).unwrap();
    let bytes = guest_elf(super::VM_GUEST_ELF_MACHINE);
    let requested = root.path().join("exact-guest-runtime");
    fs::write(&requested, &bytes).unwrap();
    let (artifact_path, artifact) = super::retain_guest_runtime_bytes(&job, &bytes).unwrap();
    let runtime_disk = nanocodex_vm::tools::GuestRuntimeDisk::prepare(
        &artifact,
        job.join(super::GUEST_RUNTIME_CACHE_ROOT),
    )
    .unwrap();
    let origin = super::RetainedGuestRuntimeOrigin {
        job: job.clone(),
        runtime: super::RetainedGuestRuntime {
            target: super::VM_GUEST_TARGET.to_owned(),
            binary_sha256: hex::encode(sha2::Sha256::digest(&bytes)),
            runtime_disk_digest: Some(runtime_disk.digest().to_owned()),
            artifact_path: Some(artifact_path),
            source: "explicit_binary".to_owned(),
            source_path: PathBuf::from("/diagnostic/source"),
            host_git_sha: "test".to_owned(),
        },
    };
    fs::remove_dir_all(job.join("guest-runtime")).unwrap();
    assert!(!artifact.parent().unwrap().exists());

    let prepared =
        super::prepare_retained_guest_runtime(&job, &origin, Some(&requested), true).unwrap();

    assert_eq!(fs::read(&artifact).unwrap(), bytes);
    assert!(prepared.disk.is_file());
    assert!(prepared.disk.starts_with(&job));
    assert_eq!(prepared.identity.unwrap(), origin.runtime);
}

#[cfg(unix)]
#[test]
fn guest_runtime_retention_rejects_a_symlink_escape_before_creating_directories() {
    let root = tempfile::tempdir().unwrap();
    let job = root.path().join("job");
    let outside = root.path().join("outside");
    fs::create_dir(&job).unwrap();
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, job.join("guest-runtime")).unwrap();

    let error = super::retain_guest_runtime_bytes(&job, &guest_elf(super::VM_GUEST_ELF_MACHINE))
        .unwrap_err();

    assert!(error.to_string().contains("escapes job"));
    assert!(!outside.join("artifacts").exists());
}

#[cfg(unix)]
#[test]
fn guest_runtime_disk_rejects_a_sibling_cache_symlink_escape() {
    let root = tempfile::tempdir().unwrap();
    let job = root.path().join("job");
    let outside = root.path().join("outside");
    fs::create_dir(&job).unwrap();
    fs::create_dir(&outside).unwrap();
    let (_, artifact) =
        super::retain_guest_runtime_bytes(&job, &guest_elf(super::VM_GUEST_ELF_MACHINE)).unwrap();
    std::os::unix::fs::symlink(&outside, job.join(super::GUEST_RUNTIME_CACHE_ROOT)).unwrap();

    let error = super::prepare_job_guest_runtime_disk(&job, &artifact).unwrap_err();

    assert!(error.to_string().contains("escapes job"));
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn guest_runtime_job_paths_reject_parent_components() {
    let job = tempfile::tempdir().unwrap();
    let path = job
        .path()
        .join(super::GUEST_RUNTIME_CACHE_ROOT)
        .join("..")
        .join("outside");

    let error = super::ensure_job_owned_path(job.path(), &path).unwrap_err();

    assert!(error.to_string().contains("escapes job"));
}

#[test]
fn retained_runtime_disk_digest_is_enforced() {
    let job = tempfile::tempdir().unwrap();
    let bytes = guest_elf(super::VM_GUEST_ELF_MACHINE);
    let (artifact_path, artifact) = super::retain_guest_runtime_bytes(job.path(), &bytes).unwrap();
    let runtime_disk = nanocodex_vm::tools::GuestRuntimeDisk::prepare(
        &artifact,
        job.path().join(super::GUEST_RUNTIME_CACHE_ROOT),
    )
    .unwrap();
    let origin = super::RetainedGuestRuntimeOrigin {
        job: job.path().to_path_buf(),
        runtime: super::RetainedGuestRuntime {
            target: super::VM_GUEST_TARGET.to_owned(),
            binary_sha256: hex::encode(sha2::Sha256::digest(&bytes)),
            runtime_disk_digest: Some("0".repeat(64)),
            artifact_path: Some(artifact_path),
            source: "explicit_binary".to_owned(),
            source_path: PathBuf::from("/diagnostic/source"),
            host_git_sha: "test".to_owned(),
        },
    };

    let error = super::prepare_retained_guest_runtime(job.path(), &origin, None, true).unwrap_err();

    assert!(error.to_string().contains(runtime_disk.digest()));
    assert!(error.to_string().contains(&"0".repeat(64)));
}

#[test]
fn guest_source_commit_must_match_the_host_binary() {
    assert!(super::validate_vm_guest_commit("abc123", "abc123").is_ok());
    let error = super::validate_vm_guest_commit("host123", "source456").unwrap_err();
    assert!(error.to_string().contains("host123"));
    assert!(error.to_string().contains("source456"));
    assert!(error.to_string().contains("--vm-guest-runtime"));
}

#[test]
fn cost_summary_distinguishes_known_and_unpriced_attempts() {
    let mut summary = RunSummary {
        total: 3,
        ..RunSummary::default()
    };

    summary.record_estimated_cost(Some(0.125), BillingCompleteness::Complete);
    summary.record_estimated_cost(None, BillingCompleteness::Complete);
    summary.record_estimated_cost(Some(0.375), BillingCompleteness::Complete);
    summary.record_estimated_cost(Some(4.304_052), BillingCompleteness::Unknown);

    assert_eq!(summary.known_estimated_cost_usd, Some(0.5));
    assert_eq!(summary.priced_attempts, 2);
    assert_eq!(
        summary.observed_estimated_cost_lower_bound_usd,
        Some(4.804_052)
    );
    assert_eq!(summary.observed_priced_attempts, 3);
    assert_eq!(summary.billing_unknown, 1);
}

#[test]
fn scored_safety_refusal_overlaps_refusal_and_error_axes() {
    let mut summary = RunSummary {
        total: 1,
        scored: 1,
        failed: 1,
        ..RunSummary::default()
    };

    summary.record_exception(Some(nanocodex_eval::EvalExceptionKind::AgentSafetyRefusal));

    assert_eq!(summary.scored, 1);
    assert_eq!(summary.unscored, 0);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.refused, 1);
    assert_eq!(summary.errored, 1);
}

#[test]
fn host_defaults_use_the_configured_share_of_cpu_and_memory() {
    let host = HostResources {
        logical_cpus: 10,
        physical_memory_bytes: Some(32 * 1024 * 1024 * 1024),
    };

    let defaults = host.scheduling_defaults(80);

    assert_eq!(defaults.concurrency, 8);
    assert_eq!(defaults.max_memory_mb, Some(26_214));
}

#[test]
fn host_defaults_keep_at_least_one_execution_slot() {
    let host = HostResources {
        logical_cpus: 1,
        physical_memory_bytes: None,
    };

    let defaults = host.scheduling_defaults(1);

    assert_eq!(defaults.concurrency, 1);
    assert_eq!(defaults.max_memory_mb, None);
}

#[test]
fn eval_open_file_limit_uses_the_available_hard_limit() {
    assert_eq!(desired_eval_open_file_limit(256, u64::MAX), 8_192);
    assert_eq!(desired_eval_open_file_limit(256, 4_096), 4_096);
    assert_eq!(desired_eval_open_file_limit(16_384, u64::MAX), 16_384);
}

#[test]
fn explicit_scheduler_limits_disable_automatic_resolution() {
    let cli = TestCli::try_parse_from([
        "nanoeval",
        "--task",
        "tasks/first",
        "--concurrency",
        "3",
        "--max-memory-mb",
        "4096",
    ])
    .unwrap();

    let resolved = cli.eval.resolve_run().unwrap();

    assert_eq!(resolved.concurrency, 3);
    assert_eq!(resolved.max_memory_mb, Some(4_096));
    assert_eq!(resolved.automatic_scheduling, None);
}

#[test]
fn resumed_workload_allows_scheduler_changes_only() {
    let retained = RunInvocation {
        version: super::INVOCATION_VERSION,
        nanocodex_build: RetainedBuild {
            version: "test".to_owned(),
            git_sha: "0123456789abcdef".to_owned(),
            built_at: "2026-07-28T00:00:00Z".to_owned(),
            executable_sha256: "abc123".to_owned(),
        },
        model: "gpt-5.6-sol".to_owned(),
        tool_profile: "microvm_workspace".to_owned(),
        seed: None,
        scheduling: RetainedScheduling {
            policy: super::SCHEDULING_POLICY.to_owned(),
            automatic_utilization_percent: Some(80),
            concurrency_source: "automatic".to_owned(),
            memory_source: "automatic".to_owned(),
        },
        trials: 5,
        concurrency: 16,
        max_memory_mb: Some(49_152),
        vm_rootfs: None,
        guest_runtime: Some(super::RetainedGuestRuntime {
            target: super::VM_GUEST_TARGET.to_owned(),
            binary_sha256: "guest123".to_owned(),
            runtime_disk_digest: Some("disk123".to_owned()),
            artifact_path: None,
            source: "explicit_binary".to_owned(),
            source_path: PathBuf::from("/opt/nanocodex-vm-guest"),
            host_git_sha: "0123456789abcdef".to_owned(),
        }),
        vm_retention: VmRetention::Failures,
        thinking: "xhigh".to_owned(),
        web_search: false,
        rerun_from: None,
    };
    let aggregate_identity = super::aggregate_run_identity(&retained);
    assert_eq!(aggregate_identity.model, "gpt-5.6-sol");
    assert_eq!(aggregate_identity.reasoning_effort, "xhigh");
    assert_eq!(aggregate_identity.tool_profile, "microvm_workspace");
    assert_eq!(
        aggregate_identity.build.executable_sha256.as_deref(),
        Some("abc123")
    );
    assert_eq!(
        aggregate_identity
            .vm
            .as_ref()
            .and_then(|vm| vm.guest_runtime_sha256.as_deref()),
        Some("guest123")
    );
    let mut resumed = retained.clone();
    resumed.concurrency = 30;
    resumed.max_memory_mb = Some(58_000);

    assert!(retained.same_workload(&resumed));

    resumed.thinking = "high".to_owned();
    assert!(!retained.same_workload(&resumed));

    let mut changed_guest = retained.clone();
    changed_guest.guest_runtime.as_mut().unwrap().binary_sha256 = "different".to_owned();
    assert!(!retained.same_workload(&changed_guest));
}

#[test]
fn passed_vm_retention_is_explicit() {
    let cli =
        TestCli::try_parse_from(["nanoeval", "--task", "tasks/first", "--vm-retention", "all"])
            .unwrap();

    assert!(cli.eval.vm_retention.unwrap().retains_passes());
}

#[test]
fn rerun_is_a_task_source_with_foundry_style_name_filters() {
    let cli = TestCli::try_parse_from([
        "nanoeval",
        "--rerun",
        "webserver",
        "--rerun-from",
        "job-id",
        "--match-task",
        "torch-.*",
        "--match-task",
        "mteb",
        "--include-errored",
        "--list",
    ])
    .unwrap();

    assert!(cli.eval.retry.rerun);
    assert_eq!(cli.eval.retry.rerun_from, Some(PathBuf::from("job-id")));
    assert_eq!(cli.eval.retry.names, ["webserver"]);
    assert_eq!(cli.eval.retry.match_task, ["torch-.*", "mteb"]);
    assert!(cli.eval.retry.statuses.include_errored);
    assert!(cli.eval.retry.list);
    assert!(cli.eval.tasks.is_empty());
    assert!(cli.eval.suites.is_empty());
}

#[test]
fn positional_rerun_names_are_literal_substrings() {
    let cli = TestCli::try_parse_from(["nanoeval", "--rerun", "task.+", "--list"]).unwrap();
    let matcher = super::retry_matcher(&cli.eval.retry).unwrap().unwrap();

    assert!(matcher.is_match("terminal-bench/task.+example"));
    assert!(!matcher.is_match("terminal-bench/taskXYZexample"));
}

#[test]
fn suite_loads_immediate_tasks_in_name_order() {
    let suite = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tasks");
    let cli = TestCli::try_parse_from([
        "nanoeval",
        "--suite",
        suite.to_str().unwrap(),
        "--concurrency",
        "3",
    ])
    .unwrap();
    let tasks = load_tasks(cli.eval.tasks, cli.eval.suites).unwrap();
    let names = tasks.iter().map(Task::name).collect::<Vec<_>>();

    assert_eq!(
        names,
        [
            "nanoeval/extract-todos",
            "nanoeval/uppercase-message",
            "nanoeval/write-greeting"
        ]
    );
}

#[test]
fn retained_task_duration_uses_the_median_completed_trial() {
    let output = tempfile::tempdir().unwrap();
    let job = output.path().join("job");
    for (trial, finished_at) in [
        ("first", "2026-07-23T00:00:10Z"),
        ("second", "2026-07-23T00:00:30Z"),
        ("third", "2026-07-23T00:00:20Z"),
    ] {
        let directory = job.join(trial);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
                directory.join("result.json"),
                format!(
                    r#"{{"task_name":"terminal-bench/example","started_at":"2026-07-23T00:00:00Z","finished_at":"{finished_at}"}}"#
                ),
            )
            .unwrap();
    }

    let estimates = retained_task_durations(output.path()).unwrap();
    assert_eq!(
        estimates["terminal-bench/example"],
        std::time::Duration::from_secs(20)
    );
}

#[test]
fn retry_selection_distinguishes_scores_refusals_and_errors() {
    let job = tempfile::tempdir().unwrap();
    super::write_json_atomic(
        &job.path().join(super::INVOCATION_FILE),
        &retained_invocation(None),
    )
    .unwrap();
    for (trial, result) in [
        (
            "passed",
            r#"{"task_name":"terminal-bench/passed","outcome":"passed","scored":true,"verifier_result":{"rewards":{"reward":1.0}},"exception_info":null}"#,
        ),
        (
            "passed-with-cleanup-failure",
            r#"{"task_name":"terminal-bench/passed-with-cleanup-failure","outcome":"passed","scored":true,"verifier_result":{"rewards":{"reward":1.0}},"exception_info":{"exception_type":"CleanupError"}}"#,
        ),
        (
            "cleanup-only",
            r#"{"task_name":"terminal-bench/cleanup-only","outcome":"infrastructure_error","scored":false,"verifier_result":null,"exception_info":{"exception_type":"CleanupError"}}"#,
        ),
        (
            "partially-failed",
            r#"{"task_name":"terminal-bench/partially-failed","outcome":"verifier_failed","scored":true,"verifier_result":{"rewards":{"first":1.0,"second":0.0}},"exception_info":null}"#,
        ),
        (
            "failed",
            r#"{"task_name":"terminal-bench/torch-failed","outcome":"verifier_failed","scored":true,"verifier_result":{"rewards":{"reward":0.0}},"exception_info":null}"#,
        ),
        (
            "refused",
            r#"{"task_name":"terminal-bench/refused","outcome":"safety_refusal","scored":false,"verifier_result":null,"exception_info":{"exception_type":"AgentSafetyRefusalError"}}"#,
        ),
        (
            "explicit-non-refusal",
            r#"{"task_name":"terminal-bench/explicit-non-refusal","outcome":"safety_refusal","scored":false,"verifier_result":null,"exception_info":{"exception_type":"VerifierError"}}"#,
        ),
        (
            "errored",
            r#"{"task_name":"terminal-bench/errored","outcome":"infrastructure_error","scored":false,"verifier_result":null,"exception_info":{"exception_type":"VerifierError"}}"#,
        ),
        (
            "scored-timeout-pass",
            r#"{"task_name":"terminal-bench/scored-timeout-pass","outcome":"agent_timeout","scored":true,"verifier_result":{"rewards":{"reward":1.0}},"exception_info":{"exception_type":"AgentTimeoutError"}}"#,
        ),
        (
            "scored-timeout-fail",
            r#"{"task_name":"terminal-bench/scored-timeout-fail","outcome":"agent_timeout","scored":true,"verifier_result":{"rewards":{"reward":0.0}},"exception_info":{"exception_type":"AgentTimeoutError"}}"#,
        ),
        (
            "unscored-with-reward",
            r#"{"task_name":"terminal-bench/unscored-with-reward","outcome":"passed","scored":false,"verifier_result":{"rewards":{"reward":1.0}},"exception_info":null}"#,
        ),
    ] {
        let directory = job.path().join(trial);
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("result.json"), result).unwrap();
    }

    let failed = retained_retry_task_names(job.path(), false, false, None).unwrap();
    assert_eq!(
        failed.task_names,
        [
            "terminal-bench/partially-failed".to_owned(),
            "terminal-bench/scored-timeout-fail".to_owned(),
            "terminal-bench/torch-failed".to_owned()
        ]
        .into()
    );

    let matcher = regex::RegexSet::new(["torch|errored|scored-timeout|refused|unscored"]).unwrap();
    let selected = retained_retry_task_names(job.path(), true, true, Some(&matcher)).unwrap();
    assert_eq!(
        selected.task_names,
        [
            "terminal-bench/errored".to_owned(),
            "terminal-bench/refused".to_owned(),
            "terminal-bench/scored-timeout-fail".to_owned(),
            "terminal-bench/torch-failed".to_owned(),
        ]
        .into()
    );

    let errors_only = retained_retry_task_names(job.path(), false, true, None).unwrap();
    assert!(errors_only.task_names.contains("terminal-bench/refused"));
    assert!(
        errors_only
            .task_names
            .contains("terminal-bench/explicit-non-refusal")
    );
    assert!(
        !errors_only
            .task_names
            .contains("terminal-bench/cleanup-only")
    );
    assert!(
        !errors_only
            .task_names
            .contains("terminal-bench/scored-timeout-pass")
    );

    let refusals_only = retained_retry_task_names(job.path(), true, false, None).unwrap();
    assert!(refusals_only.task_names.contains("terminal-bench/refused"));
    assert!(
        !refusals_only
            .task_names
            .contains("terminal-bench/explicit-non-refusal")
    );
    assert!(
        !refusals_only
            .task_names
            .contains("terminal-bench/cleanup-only")
    );
}

#[test]
fn retry_selection_uses_pass_at_k_across_trials() {
    let job = tempfile::tempdir().unwrap();
    super::write_json_atomic(
        &job.path().join(super::INVOCATION_FILE),
        &retained_invocation(None),
    )
    .unwrap();
    for (trial, task, outcome, scored, verifier_result, exception_info) in [
        (
            "eventual-pass-failed",
            "terminal-bench/eventual-pass",
            "verifier_failed",
            true,
            r#"{"rewards":{"reward":0.0}}"#,
            "null",
        ),
        (
            "eventual-pass-passed",
            "terminal-bench/eventual-pass",
            "passed",
            true,
            r#"{"rewards":{"reward":1.0}}"#,
            "null",
        ),
        (
            "scored-failure",
            "terminal-bench/scored-failure",
            "verifier_failed",
            true,
            r#"{"rewards":{"reward":0.0}}"#,
            "null",
        ),
        (
            "scored-failure-error",
            "terminal-bench/scored-failure",
            "agent_timeout",
            false,
            "null",
            r#"{"exception_type":"AgentTimeoutError"}"#,
        ),
    ] {
        let directory = job.path().join(trial);
        fs::create_dir(&directory).unwrap();
        fs::write(
                directory.join("result.json"),
                format!(
                    r#"{{"task_name":"{task}","outcome":"{outcome}","scored":{scored},"verifier_result":{verifier_result},"exception_info":{exception_info}}}"#
                ),
            )
            .unwrap();
    }

    let queue = retained_retry_task_names(job.path(), false, false, None).unwrap();

    assert_eq!(
        queue.task_names,
        ["terminal-bench/scored-failure".to_owned()].into()
    );
}

#[test]
fn retry_lineage_overlays_only_tasks_present_in_the_child_job() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("base");
    let child = root.path().join("child");
    for (job, trial, task, reward) in [
        (&base, "first", "terminal-bench/first", 0.0),
        (&base, "second", "terminal-bench/second", 0.0),
        (&child, "first-retry", "terminal-bench/first", 1.0),
    ] {
        let directory = job.join(trial);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
                directory.join("result.json"),
                format!(
                    r#"{{"task_name":"{task}","outcome":"{}","scored":true,"verifier_result":{{"rewards":{{"reward":{reward}}}}},"exception_info":null}}"#,
                    if reward > 0.0 { "passed" } else { "verifier_failed" },
                ),
            )
            .unwrap();
    }
    super::write_json_atomic(
        &base.join(super::INVOCATION_FILE),
        &retained_invocation(None),
    )
    .unwrap();
    super::write_json_atomic(
        &child.join(super::INVOCATION_FILE),
        &retained_invocation(Some(base.canonicalize().unwrap())),
    )
    .unwrap();

    let queue = retained_retry_task_names(&child, false, false, None).unwrap();

    assert_eq!(queue.lineage.len(), 2);
    assert_eq!(
        queue.task_names,
        ["terminal-bench/second".to_owned()].into()
    );
}

fn retained_invocation(rerun_from: Option<PathBuf>) -> super::RunInvocation {
    super::RunInvocation {
        version: super::INVOCATION_VERSION,
        nanocodex_build: super::RetainedBuild {
            version: "test".to_owned(),
            git_sha: "0123456789abcdef".to_owned(),
            built_at: "2026-07-28T00:00:00Z".to_owned(),
            executable_sha256: "abc123".to_owned(),
        },
        model: "gpt-5.6-sol".to_owned(),
        tool_profile: "microvm_workspace".to_owned(),
        seed: None,
        scheduling: super::RetainedScheduling {
            policy: super::SCHEDULING_POLICY.to_owned(),
            automatic_utilization_percent: None,
            concurrency_source: "configured".to_owned(),
            memory_source: "configured".to_owned(),
        },
        trials: 1,
        concurrency: 1,
        max_memory_mb: None,
        vm_rootfs: None,
        guest_runtime: None,
        vm_retention: super::VmRetention::Failures,
        thinking: "low".to_owned(),
        web_search: false,
        rerun_from,
    }
}

#[test]
fn requires_at_least_one_task() {
    let Err(error) = TestCli::try_parse_from(["nanoeval"]) else {
        panic!("a task should be required");
    };
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
}

fn guest_elf(machine: u16) -> Vec<u8> {
    let mut bytes = vec![0_u8; 64];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    bytes
}

fn write_test_task(root: &Path) -> Task {
    fs::create_dir_all(root.join("environment")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(root.join("instruction.md"), "Complete the task.\n").unwrap();
    fs::write(root.join("tests/test.sh"), "exit 0\n").unwrap();
    fs::write(
        root.join("task.toml"),
        r#"
schema_version = "1.1"
[task]
name = "terminal-bench/runtime-resume"
description = "runtime resume fixture"
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
"#,
    )
    .unwrap();
    Task::load(root).unwrap()
}
