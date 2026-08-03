use std::convert::Infallible;

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
};

use nanocodex_agent::{Nanocodex, OpenAi, events::AgentEventKind};
use nanocodex_oai_api::MODEL;
use tempfile::tempdir;

use crate::{
    AgentStatus, AtifStep, AtifTrajectory, EvalAttemptOutcome, EvalEventKind, EvalOutcome,
    EvalStatus, VerifierResult,
    evaluator::{AdmissionController, AttemptAgent},
};

use super::{
    ApiEventLoopTailSummary, ApiRequestPayload, ApiTokenUsageSummary, ArmReport, ArmStatus,
    ArmSummary, CodexExec, CodexToolMode, CodexVersion, DIFF_CODEX_CA_BUNDLE_FILENAME,
    DIFF_CODEX_CLOUD_CONFIG_CACHE_FILENAME, DIFF_CODEX_SSL_CERT_FILE_ENVIRONMENT,
    DetectedEmptyStdinCalls, DiffCodexCaSource, DiffProgress, DifferentialBuildError,
    DifferentialClassification, DifferentialEvaluator, DifferentialMemoryPlanner,
    DifferentialMemoryProfile, DifferentialMemoryProfiles, DifferentialProfile,
    DifferentialReportSummary, DifferentialSweepManifest, DifferentialSweepProfile,
    DifferentialSweepTask, Evaluator, InfrastructureReplacementState, LaneProgressState,
    LiveApiArm, NanocodexToolMode, ScheduledComparison, ShellPollingSummary, Task,
    TrajectoryProjection, build_event_loop_trace, capture_proxy_vm_base_url,
    cleanup_incomplete_differential_upper_disks, compare_api_exchanges,
    create_durable_comparison_directory_with_sync, detected_code_mode_empty_stdin_calls,
    detected_polling_turn, diff_json, differential_comparison_name, differential_pair_memory_mb,
    differential_sweep_profiles, event_loop_difference_categories, heartbeat_needed,
    heartbeat_summary, initial_differential_schedule, inspect_api_exchanges,
    join_differential_arms, memory_with_slack, newly_completed_lines, next_guest_memory_after_oom,
    normalize_retained_arm_tool_calls, read_api_request_payloads,
    read_optional_codex_cloud_config_cache, reanalyze, releasable_differential_arm_memory_mb,
    resume_differential_schedule, retained_differential_summary, run_arm,
    stage_diff_codex_ca_bundle, summarize_nanocodex, task_lane_available,
    validate_differential_profile, validate_differential_profiles, write_json_atomic,
    write_json_atomic_with_sync,
};

#[test]
fn capture_proxy_uses_the_direct_gvproxy_host_route() {
    assert_eq!(
        capture_proxy_vm_base_url(4312),
        "http://192.168.127.254:4312"
    );
}

#[test]
fn differential_completion_syncs_each_published_directory_entry() {
    let output = tempdir().unwrap();
    let comparison = output.path().join("comparison");
    let mut synced = Vec::new();
    create_durable_comparison_directory_with_sync(output.path(), &comparison, |directory| {
        synced.push(directory.to_path_buf());
        Ok(())
    })
    .unwrap();
    assert_eq!(synced, [output.path()]);

    synced.clear();
    let report = comparison.join("comparison.json");
    write_json_atomic_with_sync(
        &report,
        &serde_json::json!({"complete": true}),
        |directory| {
            synced.push(directory.to_path_buf());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(synced, [comparison]);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(report).unwrap()).unwrap(),
        serde_json::json!({"complete": true})
    );
}

#[tokio::test]
async fn differential_scheduler_rejects_zero_limits_before_asset_work() {
    let nanocodex = Nanocodex::builder(OpenAi::new("test").unwrap());
    assert!(matches!(
        DifferentialEvaluator::builder(nanocodex.clone())
            .max_concurrency(0)
            .prepare()
            .await,
        Err(DifferentialBuildError::InvalidConcurrency)
    ));
    assert!(matches!(
        DifferentialEvaluator::builder(nanocodex)
            .max_memory_mb(0)
            .prepare()
            .await,
        Err(DifferentialBuildError::InvalidMemory)
    ));
}

#[test]
fn resumed_sweep_removes_only_interrupted_comparison_upper_disks() {
    use std::os::unix::fs::symlink;

    let output = tempdir().unwrap();
    let interrupted = output.path().join("interrupted-comparison");
    let completed = output.path().join("completed-comparison");
    let unrelated = output.path().join("unrelated-assets");
    let outside = tempdir().unwrap();
    let outside_upper = outside.path().join("outside.upper.ext4");

    fs::create_dir_all(interrupted.join("nanocodex/attempt")).unwrap();
    fs::create_dir_all(interrupted.join("codex/attempt")).unwrap();
    fs::write(interrupted.join(super::PROGRESS_FILE), b"started\n").unwrap();
    fs::write(
        interrupted.join("nanocodex/attempt/rootfs.upper.ext4"),
        b"nanocodex",
    )
    .unwrap();
    fs::write(
        interrupted.join("codex/attempt/rootfs.upper.ext4"),
        b"codex",
    )
    .unwrap();
    fs::write(interrupted.join("codex/attempt/keep.txt"), b"evidence").unwrap();
    fs::write(&outside_upper, b"outside").unwrap();
    symlink(
        &outside_upper,
        interrupted.join("codex/attempt/linked.upper.ext4"),
    )
    .unwrap();

    fs::create_dir_all(completed.join("nanocodex/attempt")).unwrap();
    fs::write(completed.join(super::PROGRESS_FILE), b"completed\n").unwrap();
    fs::write(completed.join(super::COMPARISON_FILE), b"{}").unwrap();
    let completed_upper = completed.join("nanocodex/attempt/rootfs.upper.ext4");
    fs::write(&completed_upper, b"completed").unwrap();

    fs::create_dir_all(&unrelated).unwrap();
    let unrelated_upper = unrelated.join("asset.upper.ext4");
    fs::write(&unrelated_upper, b"asset").unwrap();

    assert_eq!(
        cleanup_incomplete_differential_upper_disks(output.path()).unwrap(),
        2
    );
    assert!(
        !interrupted
            .join("nanocodex/attempt/rootfs.upper.ext4")
            .exists()
    );
    assert!(!interrupted.join("codex/attempt/rootfs.upper.ext4").exists());
    assert!(interrupted.join("codex/attempt/keep.txt").is_file());
    assert!(
        interrupted
            .join("codex/attempt/linked.upper.ext4")
            .is_symlink()
    );
    assert!(outside_upper.is_file());
    assert!(completed_upper.is_file());
    assert!(unrelated_upper.is_file());
}

#[test]
fn differential_coordinates_charge_both_arms_and_name_the_trial() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let id = uuid::Uuid::from_u128(0x1234);

    assert_eq!(differential_pair_memory_mb(task.resources().memory_mb), 512);
    assert_eq!(
        releasable_differential_arm_memory_mb(256, 512, Some(512)),
        256
    );
    assert_eq!(
        releasable_differential_arm_memory_mb(256, 512, Some(511)),
        0
    );
    assert_eq!(releasable_differential_arm_memory_mb(256, 512, None), 0);
    assert_eq!(
        differential_comparison_name(
            &task,
            DifferentialProfile::new(
                nanocodex_agent::Thinking::Medium,
                NanocodexToolMode::CodeModeOnly,
                CodexToolMode::CodeModeOnly,
            ),
            5,
            id,
        ),
        format!(
            "write-greeting__medium__nanocodex_code_mode_only__codex_code_mode_only__005__{}",
            id.simple()
        )
    );
}

#[test]
fn differential_scheduler_caps_task_lanes_and_keeps_one_pair_per_task() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let scheduled = ScheduledComparison {
        task_index: 7,
        profile_index: 0,
        task,
        trial: 1,
        profile: DifferentialProfile::new(
            nanocodex_agent::Thinking::High,
            NanocodexToolMode::CodeMode,
            CodexToolMode::CodeMode,
        ),
        infrastructure_replacement_for: None,
        memory_attempt: 1,
        minimum_guest_memory_mb: None,
        memory_retry_for: None,
        queued_at: chrono::Utc::now(),
    };
    let mut active_tasks = std::collections::BTreeSet::new();
    let max_active_tasks = 1;

    assert!(task_lane_available(
        &active_tasks,
        &scheduled,
        max_active_tasks
    ));
    assert!(active_tasks.insert(scheduled.task_index));
    assert!(!task_lane_available(
        &active_tasks,
        &scheduled,
        max_active_tasks
    ));

    let mut other_task = scheduled.clone();
    other_task.task_index += 1;
    assert!(!task_lane_available(
        &active_tasks,
        &other_task,
        max_active_tasks
    ));

    assert!(active_tasks.remove(&scheduled.task_index));
    assert!(task_lane_available(
        &active_tasks,
        &other_task,
        max_active_tasks
    ));
}

#[test]
fn infrastructure_outcome_makes_the_comparison_incomplete() {
    fn arm(status: ArmStatus, outcome: EvalOutcome) -> ArmReport {
        let mut summary = ArmSummary::runner_error();
        summary.status = status;
        summary.outcome = Some(outcome);
        ArmReport {
            summary,
            evaluator_directory: None,
            event_log: None,
            trajectory: None,
            trajectory_summary: None,
            trajectory_error: None,
            api_exchanges: None,
            api_capture: None,
            api_capture_error: None,
            codex_events: None,
            codex_stderr: None,
            codex_summary: None,
            operational_error: None,
            event_error: None,
            memory: None,
            outcome: None,
        }
    }

    let infrastructure = arm(ArmStatus::Unscored, EvalOutcome::InfrastructureError);
    let passed = arm(ArmStatus::Passed, EvalOutcome::Passed);

    assert!(matches!(
        DifferentialClassification::from_arms(&infrastructure, &passed),
        DifferentialClassification::Incomplete
    ));
}

#[test]
fn adaptive_memory_uses_persisted_arm_measurements_and_geometric_oom_growth() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = cache.path().join("memory.json");
    let profile = DifferentialMemoryProfile {
        task_name: task.name().to_owned(),
        content_digest: task.content_digest().to_owned(),
        guest_memory_mb: 192,
        nanocodex_admission_memory_mb: 144,
        codex_admission_memory_mb: 176,
        oom_floor_guest_memory_mb: 192,
        nanocodex_host_peak_rss_mib: Some(66),
        codex_host_peak_rss_mib: Some(91),
        guest_peak_used_mib: Some(104),
        updated_at: chrono::Utc::now(),
    };
    write_json_atomic(
        &path,
        &DifferentialMemoryProfiles {
            schema_version: super::MEMORY_PROFILE_SCHEMA_VERSION,
            tasks: std::collections::BTreeMap::from([(task.content_digest().to_owned(), profile)]),
        },
    )
    .unwrap();

    let planner = DifferentialMemoryPlanner::load(path, 64).unwrap();
    let plan = planner.plan(&task, None);
    assert_eq!(plan.guest_memory_mb, 192);
    assert_eq!(plan.nanocodex_admission_memory_mb, 144);
    assert_eq!(plan.codex_admission_memory_mb, 176);
    assert_eq!(plan.pair_admission_memory_mb(), 320);
    assert_eq!(memory_with_slack(100), 184);
    assert_eq!(next_guest_memory_after_oom(128, 1_024), Some(256));
    assert_eq!(next_guest_memory_after_oom(768, 1_024), Some(1_024));
    assert_eq!(next_guest_memory_after_oom(1_024, 1_024), None);
}

#[test]
fn resumed_oom_requeues_the_same_pair_without_spending_replacement_budget() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let profile = DifferentialProfile::new(
        nanocodex_agent::Thinking::Medium,
        NanocodexToolMode::CodeModeOnly,
        CodexToolMode::CodeModeOnly,
    );
    let (mut replacements, mut pending) =
        initial_differential_schedule(vec![task.clone()], 1, &[profile], 2);
    let summary = DifferentialReportSummary {
        task_name: task.name().to_owned(),
        task_root: task.root().to_path_buf(),
        task_content_digest: task.content_digest().to_owned(),
        trial: 1,
        thinking: profile.thinking().as_str().to_owned(),
        nanocodex_tool_mode: profile.nanocodex_tool_mode(),
        codex_tool_mode: profile.codex_tool_mode(),
        classification: DifferentialClassification::Incomplete,
        infrastructure_failure: true,
        operational_error: false,
        oom_detected: true,
        memory_attempt: 1,
        configured_guest_memory_mb: 128,
        declared_guest_memory_mb: 256,
        infrastructure_replacement_for: None,
        comparison_path: Path::new("retained/oom/comparison.json").to_path_buf(),
    };

    let skipped = resume_differential_schedule(
        &mut pending,
        &mut replacements,
        std::slice::from_ref(&summary),
        1,
        2,
        1,
    );
    assert_eq!(skipped, 0);
    assert_eq!(pending.len(), 1);
    let retry = pending.pop_front().unwrap();
    assert_eq!(retry.trial, 1);
    assert_eq!(retry.memory_attempt, 2);
    assert_eq!(retry.minimum_guest_memory_mb, Some(256));
    assert_eq!(replacements[0].remaining, 2);

    let mut exhausted = summary;
    exhausted.memory_attempt = 2;
    exhausted.configured_guest_memory_mb = 256;
    let (mut replacements, mut pending) =
        initial_differential_schedule(vec![task], 1, &[profile], 2);
    resume_differential_schedule(&mut pending, &mut replacements, &[exhausted], 1, 2, 1);
    assert!(pending.is_empty());
    assert_eq!(replacements[0].remaining, 2);
}

#[test]
fn resumed_task_lane_starts_at_first_missing_trial_without_reordering_profiles() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let profiles = [
        DifferentialProfile::new(
            nanocodex_agent::Thinking::High,
            NanocodexToolMode::CodeMode,
            CodexToolMode::CodeMode,
        ),
        DifferentialProfile::new(
            nanocodex_agent::Thinking::High,
            NanocodexToolMode::CodeModeOnly,
            CodexToolMode::CodeModeOnly,
        ),
    ];
    let completed = (1..=6)
        .map(|trial| DifferentialReportSummary {
            task_name: task.name().to_owned(),
            task_root: task.root().to_path_buf(),
            task_content_digest: task.content_digest().to_owned(),
            trial,
            thinking: profiles[0].thinking().as_str().to_owned(),
            nanocodex_tool_mode: profiles[0].nanocodex_tool_mode(),
            codex_tool_mode: profiles[0].codex_tool_mode(),
            classification: DifferentialClassification::BothPassed,
            infrastructure_failure: false,
            operational_error: false,
            oom_detected: false,
            memory_attempt: 1,
            configured_guest_memory_mb: 256,
            declared_guest_memory_mb: 256,
            infrastructure_replacement_for: None,
            comparison_path: PathBuf::from(format!("retained/cm-{trial}/comparison.json")),
        })
        .collect::<Vec<_>>();
    let (mut replacements, mut pending) =
        initial_differential_schedule(vec![task], 10, &profiles, 0);

    let skipped =
        resume_differential_schedule(&mut pending, &mut replacements, &completed, 10, 0, 2);
    let remaining = pending
        .into_iter()
        .map(|scheduled| (scheduled.profile_index, scheduled.trial))
        .collect::<Vec<_>>();
    let expected = (7..=10)
        .map(|trial| (0, trial))
        .chain((1..=10).map(|trial| (1, trial)))
        .collect::<Vec<_>>();

    assert_eq!(skipped, 6);
    assert_eq!(remaining, expected);
    assert_eq!(replacements[0].valid, 6);
    assert_eq!(replacements[0].outstanding, 4);
    assert_eq!(replacements[1].valid, 0);
    assert_eq!(replacements[1].outstanding, 10);
}

#[test]
fn resumed_invalid_pair_uses_fresh_bounded_replacement_lineage() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let profile = DifferentialProfile::new(
        nanocodex_agent::Thinking::Medium,
        NanocodexToolMode::CodeModeOnly,
        CodexToolMode::CodeModeOnly,
    );
    let failed = DifferentialReportSummary {
        task_name: task.name().to_owned(),
        task_root: task.root().to_path_buf(),
        task_content_digest: task.content_digest().to_owned(),
        trial: 1,
        thinking: profile.thinking().as_str().to_owned(),
        nanocodex_tool_mode: profile.nanocodex_tool_mode(),
        codex_tool_mode: profile.codex_tool_mode(),
        classification: DifferentialClassification::Incomplete,
        infrastructure_failure: true,
        operational_error: false,
        oom_detected: false,
        memory_attempt: 1,
        configured_guest_memory_mb: 128,
        declared_guest_memory_mb: 256,
        infrastructure_replacement_for: None,
        comparison_path: Path::new("retained/infrastructure/comparison.json").to_path_buf(),
    };
    let (mut replacements, mut pending) =
        initial_differential_schedule(vec![task.clone()], 1, &[profile], 2);

    resume_differential_schedule(
        &mut pending,
        &mut replacements,
        std::slice::from_ref(&failed),
        1,
        2,
        1,
    );

    assert_eq!(pending.len(), 1);
    let replacement = pending.pop_front().unwrap();
    assert_eq!(replacement.trial, 2);
    assert_eq!(replacement.infrastructure_replacement_for, Some(1));
    assert_eq!(replacements[0].remaining, 1);

    let mut operational_failure = failed.clone();
    operational_failure.infrastructure_failure = false;
    operational_failure.operational_error = true;
    operational_failure.comparison_path =
        Path::new("retained/operational/comparison.json").to_path_buf();
    let (mut operational_replacements, mut operational_pending) =
        initial_differential_schedule(vec![task.clone()], 1, &[profile], 2);
    resume_differential_schedule(
        &mut operational_pending,
        &mut operational_replacements,
        std::slice::from_ref(&operational_failure),
        1,
        2,
        1,
    );
    assert_eq!(operational_pending.len(), 1);
    let operational_replacement = operational_pending.pop_front().unwrap();
    assert_eq!(operational_replacement.trial, 2);
    assert_eq!(
        operational_replacement.infrastructure_replacement_for,
        Some(1)
    );
    assert_eq!(operational_replacements[0].remaining, 1);

    let mut completed_replacement = failed.clone();
    completed_replacement.trial = 2;
    completed_replacement.classification = DifferentialClassification::BothPassed;
    completed_replacement.infrastructure_failure = false;
    completed_replacement.infrastructure_replacement_for = Some(1);
    completed_replacement.comparison_path =
        Path::new("retained/replacement/comparison.json").to_path_buf();
    let (mut replacements, mut pending) =
        initial_differential_schedule(vec![task], 1, &[profile], 2);
    resume_differential_schedule(
        &mut pending,
        &mut replacements,
        &[failed, completed_replacement],
        1,
        2,
        1,
    );
    assert!(pending.is_empty());
    assert_eq!(replacements[0].remaining, 1);
}

#[test]
fn retained_sweep_summary_restores_memory_retry_evidence() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let retained = tempfile::tempdir().unwrap();
    let comparison_path = retained.path().join("comparison.json");
    let manifest = DifferentialSweepManifest {
        schema_version: super::SWEEP_MANIFEST_SCHEMA_VERSION,
        comparison_schema_version: super::COMPARISON_SCHEMA_VERSION,
        model: MODEL.to_owned(),
        web_search: false,
        trials: 1,
        tasks: vec![DifferentialSweepTask {
            name: task.name().to_owned(),
            root: task.root().to_path_buf(),
            content_digest: task.content_digest().to_owned(),
        }],
        profiles: vec![DifferentialSweepProfile {
            thinking: "medium".to_owned(),
            nanocodex_tool_mode: "code_mode_only".to_owned(),
            codex_tool_mode: "code_mode_only".to_owned(),
        }],
        nanocodex_sha256: "nano-sha".to_owned(),
        codex_sha256: "codex-sha".to_owned(),
    };
    let clean_arm = serde_json::json!({
        "operational_error": null,
        "event_error": null,
        "trajectory_error": null,
        "api_capture_error": null,
        "memory": {
            "host_peak_rss_mib": 200,
            "guest_total_mib": 128,
            "guest_peak_used_mib": 127,
            "guest_oom_kills": 1,
            "oom_detected": true
        },
        "outcome": null
    });
    let report = serde_json::json!({
        "schema_version": super::COMPARISON_SCHEMA_VERSION,
        "task": {
            "name": task.name(),
            "root": task.root(),
            "content_digest": task.content_digest()
        },
        "trial": 1,
        "model": MODEL,
        "thinking": "medium",
        "policy": {
            "web_search": false,
            "nanocodex_tool_mode": "code_mode_only",
            "codex_tool_mode": "code_mode_only"
        },
        "schedule": {
            "queued_at": "2026-07-29T00:00:00Z",
            "admitted_at": "2026-07-29T00:00:01Z",
            "queue_duration_ms": 1000,
            "declared_pair_memory_mb": 512,
            "requested_pair_memory_mb": 256,
            "admitted_pair_memory_mb": 256,
            "configured_guest_memory_mb": 128,
            "nanocodex_admission_memory_mb": 128,
            "codex_admission_memory_mb": 128,
            "memory_attempt": 1,
            "memory_retry_for": null,
            "max_concurrency": 40,
            "max_memory_mb": 52428,
            "max_infrastructure_replacements": 1,
            "infrastructure_replacement_for": null
        },
        "classification": "incomplete",
        "nanocodex_build": { "sha256": "nano-sha" },
        "codex_build": { "sha256": "codex-sha" },
        "nanocodex": clean_arm,
        "codex": {
            "operational_error": null,
            "event_error": null,
            "trajectory_error": null,
            "api_capture_error": null,
            "memory": null,
            "outcome": null
        },
        "artifacts": {
            "comparison": comparison_path,
            "progress_error": null,
            "api_comparison_error": null,
            "profile_validation_error": null
        }
    });
    write_json_atomic(&comparison_path, &report).unwrap();

    let summary = retained_differential_summary(&comparison_path, &manifest).unwrap();
    assert_eq!(
        summary.nanocodex_tool_mode(),
        NanocodexToolMode::CodeModeOnly
    );
    assert!(summary.oom_detected());
    assert!(summary.has_infrastructure_failure());
    assert_eq!(summary.memory_attempt(), 1);
    assert_eq!(summary.configured_guest_memory_mb(), 128);
    assert_eq!(summary.infrastructure_replacement_for, None);
}

#[test]
fn infrastructure_replacements_use_fresh_bounded_trial_coordinates() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let mut replacements = InfrastructureReplacementState {
        task,
        profile: DifferentialProfile::new(
            nanocodex_agent::Thinking::High,
            NanocodexToolMode::CodeModeOnly,
            CodexToolMode::CodeMode,
        ),
        next_trial: 6,
        remaining: 5,
        target_valid: 5,
        valid: 0,
        outstanding: 0,
    };

    let coordinates = (1..=6)
        .map(|failed_trial| {
            replacements.next(3, 1, failed_trial).map(|scheduled| {
                (
                    scheduled.task_index,
                    scheduled.profile_index,
                    scheduled.trial,
                    scheduled.profile,
                    scheduled.infrastructure_replacement_for,
                )
            })
        })
        .collect::<Vec<_>>();

    assert_eq!(
        coordinates,
        vec![
            Some((
                3,
                1,
                6,
                DifferentialProfile::new(
                    nanocodex_agent::Thinking::High,
                    NanocodexToolMode::CodeModeOnly,
                    CodexToolMode::CodeMode
                ),
                Some(1)
            )),
            Some((
                3,
                1,
                7,
                DifferentialProfile::new(
                    nanocodex_agent::Thinking::High,
                    NanocodexToolMode::CodeModeOnly,
                    CodexToolMode::CodeMode
                ),
                Some(2)
            )),
            Some((
                3,
                1,
                8,
                DifferentialProfile::new(
                    nanocodex_agent::Thinking::High,
                    NanocodexToolMode::CodeModeOnly,
                    CodexToolMode::CodeMode
                ),
                Some(3)
            )),
            Some((
                3,
                1,
                9,
                DifferentialProfile::new(
                    nanocodex_agent::Thinking::High,
                    NanocodexToolMode::CodeModeOnly,
                    CodexToolMode::CodeMode
                ),
                Some(4)
            )),
            Some((
                3,
                1,
                10,
                DifferentialProfile::new(
                    nanocodex_agent::Thinking::High,
                    NanocodexToolMode::CodeModeOnly,
                    CodexToolMode::CodeMode
                ),
                Some(5)
            )),
            None,
        ]
    );
}

#[test]
fn infrastructure_replacement_only_fills_an_uncovered_target_slot() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let mut replacements = InfrastructureReplacementState {
        task,
        profile: DifferentialProfile::new(
            nanocodex_agent::Thinking::Medium,
            NanocodexToolMode::CodeModeOnly,
            CodexToolMode::CodeModeOnly,
        ),
        next_trial: 6,
        remaining: 5,
        target_valid: 5,
        valid: 3,
        outstanding: 2,
    };

    assert!(replacements.next(0, 0, 1).is_none());
    assert_eq!(replacements.remaining, 5);

    replacements.complete(false);
    let replacement = replacements.next(0, 0, 2).unwrap();
    assert_eq!(replacement.trial, 6);
    assert_eq!(replacement.infrastructure_replacement_for, Some(2));
    assert_eq!(replacements.valid, 3);
    assert_eq!(replacements.outstanding, 2);
    assert_eq!(replacements.remaining, 4);

    replacements.complete(true);
    assert!(replacements.next(0, 0, 3).is_none());
    assert_eq!(replacements.valid, 4);
    assert_eq!(replacements.outstanding, 1);
    assert_eq!(replacements.remaining, 4);
}

#[test]
fn differential_matrix_requires_distinct_tool_modes() {
    assert!(
        validate_differential_profiles(&[
            DifferentialProfile::new(
                nanocodex_agent::Thinking::Low,
                NanocodexToolMode::CodeModeOnly,
                CodexToolMode::CodeMode,
            ),
            DifferentialProfile::new(
                nanocodex_agent::Thinking::Low,
                NanocodexToolMode::CodeModeOnly,
                CodexToolMode::CodeModeOnly,
            ),
        ])
        .is_ok()
    );
    assert_eq!(
        validate_differential_profiles(&[]).unwrap_err().to_string(),
        "differential matrix requires at least one profile"
    );
    let duplicate = DifferentialProfile::new(
        nanocodex_agent::Thinking::Low,
        NanocodexToolMode::CodeModeOnly,
        CodexToolMode::CodeModeOnly,
    );
    assert_eq!(
        validate_differential_profiles(&[duplicate, duplicate])
            .unwrap_err()
            .to_string(),
        "differential matrix contains duplicate profile low__nanocodex_code_mode_only__codex_code_mode_only"
    );
}

#[test]
fn differential_matrix_expands_profiles_through_one_queue() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let profiles = [
        DifferentialProfile::new(
            nanocodex_agent::Thinking::Medium,
            NanocodexToolMode::CodeMode,
            CodexToolMode::CodeMode,
        ),
        DifferentialProfile::new(
            nanocodex_agent::Thinking::High,
            NanocodexToolMode::CodeModeOnly,
            CodexToolMode::CodeModeOnly,
        ),
    ];

    let (replacements, pending) = initial_differential_schedule(vec![task], 2, &profiles, 2);
    let coordinates = pending
        .into_iter()
        .map(|scheduled| {
            (
                scheduled.task_index,
                scheduled.profile_index,
                scheduled.trial,
                scheduled.profile,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        coordinates,
        [
            (0, 0, 1, profiles[0]),
            (0, 0, 2, profiles[0]),
            (0, 1, 1, profiles[1]),
            (0, 1, 2, profiles[1]),
        ]
    );
    assert_eq!(replacements.len(), 2);
    assert_eq!(replacements[0].profile, profiles[0]);
    assert_eq!(replacements[1].profile, profiles[1]);
    assert!(replacements.iter().all(|state| state.remaining == 2));
}

#[test]
fn differential_manifest_profile_order_is_execution_identity() {
    let profiles = [
        DifferentialProfile::new(
            nanocodex_agent::Thinking::High,
            NanocodexToolMode::CodeMode,
            CodexToolMode::CodeMode,
        ),
        DifferentialProfile::new(
            nanocodex_agent::Thinking::High,
            NanocodexToolMode::CodeModeOnly,
            CodexToolMode::CodeModeOnly,
        ),
    ];
    let retained = differential_sweep_profiles(&profiles);
    let reversed = differential_sweep_profiles(&profiles.into_iter().rev().collect::<Vec<_>>());

    assert_eq!(retained[0].nanocodex_tool_mode, "code_mode");
    assert_eq!(retained[1].nanocodex_tool_mode, "code_mode_only");
    assert_ne!(retained, reversed);
}

#[test]
fn differential_backfills_after_two_independent_arms_finish() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let admission = std::sync::Arc::new(AdmissionController::new(4, Some(8)));
        let first_pair = admission.acquire_many(2, 4).await.unwrap();
        let second_pair = admission.acquire_many(2, 4).await.unwrap();
        let (first_nanocodex_send, first_nanocodex_receive) = tokio::sync::oneshot::channel();
        let (first_codex_send, first_codex_receive) = tokio::sync::oneshot::channel();
        let (second_nanocodex_send, second_nanocodex_receive) = tokio::sync::oneshot::channel();
        let (second_codex_send, second_codex_receive) = tokio::sync::oneshot::channel();
        let first_joined = tokio::spawn(join_differential_arms(
            first_pair,
            2,
            2,
            async move { first_nanocodex_receive.await.unwrap() },
            async move { first_codex_receive.await.unwrap() },
        ));
        let second_joined = tokio::spawn(join_differential_arms(
            second_pair,
            2,
            2,
            async move { second_nanocodex_receive.await.unwrap() },
            async move { second_codex_receive.await.unwrap() },
        ));

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(5),
                admission.acquire_many(2, 4)
            )
            .await
            .is_err()
        );
        first_nanocodex_send.send("first-nanocodex").unwrap();
        second_codex_send.send("second-codex").unwrap();
        let backfill = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            admission.acquire_many(2, 4),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!first_joined.is_finished());
        assert!(!second_joined.is_finished());

        drop(backfill);
        first_codex_send.send("first-codex").unwrap();
        second_nanocodex_send.send("second-nanocodex").unwrap();
        assert_eq!(
            first_joined.await.unwrap(),
            ("first-nanocodex", "first-codex")
        );
        assert_eq!(
            second_joined.await.unwrap(),
            ("second-nanocodex", "second-codex")
        );
    });
}

#[test]
fn reanalysis_keeps_missing_refusal_trajectories_unavailable() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("comparison.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "model": "gpt-5.6-sol",
            "thinking": "medium",
            "nanocodex": {
                "trajectory": null
            },
            "codex": {
                "trajectory": null
            },
            "artifacts": {}
        }))
        .unwrap(),
    )
    .unwrap();

    let rebuilt = reanalyze(directory.path()).unwrap();

    assert_eq!(
        rebuilt
            .comparison()
            .pointer("/trajectory_comparison/comparable"),
        Some(&serde_json::json!(false))
    );
    assert_eq!(
        rebuilt
            .comparison()
            .pointer("/nanocodex/trajectory_summary"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        rebuilt.comparison().pointer("/codex/trajectory_summary"),
        Some(&serde_json::Value::Null)
    );
    assert!(
        rebuilt
            .human_summary()
            .contains("codex trajectory: unavailable")
    );
}

#[test]
fn codex_auth_stages_only_the_adjacent_cloud_config_cache() {
    let codex_home = tempfile::tempdir().unwrap();
    let auth_file = codex_home.path().join("auth.json");
    fs::write(&auth_file, b"auth").unwrap();
    assert_eq!(
        read_optional_codex_cloud_config_cache(&auth_file).unwrap(),
        None
    );

    let cache_file = codex_home
        .path()
        .join(DIFF_CODEX_CLOUD_CONFIG_CACHE_FILENAME);
    fs::write(&cache_file, b"signed cloud config").unwrap();
    fs::write(codex_home.path().join("config.toml"), b"ignored").unwrap();
    assert_eq!(
        read_optional_codex_cloud_config_cache(&auth_file).unwrap(),
        Some(b"signed cloud config".to_vec())
    );
}

#[test]
fn codex_ca_bundle_is_staged_read_only() {
    let source_directory = tempfile::tempdir().unwrap();
    let source = source_directory.path().join("host-ca.pem");
    fs::write(&source, b"host CA bundle").unwrap();
    let share = tempfile::tempdir().unwrap();

    let staged = stage_diff_codex_ca_bundle(
        &DiffCodexCaSource {
            path: source,
            source_environment: "test",
            guest_environment: DIFF_CODEX_SSL_CERT_FILE_ENVIRONMENT,
        },
        share.path(),
    )
    .unwrap();
    let staged_path = share.path().join(DIFF_CODEX_CA_BUNDLE_FILENAME);
    assert_eq!(
        staged.guest_environment,
        DIFF_CODEX_SSL_CERT_FILE_ENVIRONMENT
    );
    assert_eq!(fs::read(&staged_path).unwrap(), b"host CA bundle");
    assert_eq!(
        fs::metadata(staged_path).unwrap().permissions().mode() & 0o777,
        0o444
    );
}

#[test]
fn codex_progress_waits_for_complete_jsonl_records() {
    let (lines, offset) = newly_completed_lines(b"first\nsecond", 0, false);
    assert_eq!(lines, [b"first".as_slice()]);
    assert_eq!(offset, 6);

    let (lines, offset) = newly_completed_lines(b"first\nsecond\nthird\n", offset, false);
    assert_eq!(lines, [b"second".as_slice(), b"third".as_slice()]);
    assert_eq!(offset, 19);

    let (lines, offset) = newly_completed_lines(b"first\nsecond\nthird\nfinal", offset, true);
    assert_eq!(lines, [b"final".as_slice()]);
    assert_eq!(offset, 24);
}

#[test]
fn heartbeat_reports_each_lanes_last_observed_state() {
    let mut lanes = std::collections::BTreeMap::new();
    assert!(heartbeat_needed(&lanes));
    lanes.insert(
        "nanocodex",
        LaneProgressState {
            elapsed_ms: 10_000,
            kind: "model.call.started".to_owned(),
            summary: Some("call 8".to_owned()),
        },
    );
    lanes.insert(
        "codex",
        LaneProgressState {
            elapsed_ms: 15_000,
            kind: "command_execution.started".to_owned(),
            summary: Some("apt-get install r-base".to_owned()),
        },
    );

    assert!(heartbeat_needed(&lanes));
    assert_eq!(
        heartbeat_summary(&lanes, 25_000),
        "nanocodex: model.call.started (call 8) for 15.0s · codex: command_execution.started (apt-get install r-base) for 10.0s"
    );

    lanes.get_mut("nanocodex").unwrap().kind = "attempt.completed".to_owned();
    lanes.get_mut("codex").unwrap().kind = "attempt.completed".to_owned();
    assert!(!heartbeat_needed(&lanes));
}

#[test]
fn live_api_diff_ignores_stream_churn_until_a_terminal_boundary() {
    let mut arm = LiveApiArm::default();
    assert!(arm.observe(
        "outbound",
        "generation",
        Some(7),
        &serde_json::json!({"type": "response.create", "input": []}),
    ));
    for delta in 0..10_000 {
        assert!(!arm.observe(
            "inbound",
            "generation",
            Some(7),
            &serde_json::json!({
                "type": "response.output_text.delta",
                "delta": delta.to_string(),
            }),
        ));
    }
    assert!(arm.turns[0].response_events.is_empty());
    assert!(arm.observe(
        "inbound",
        "generation",
        Some(7),
        &serde_json::json!({
            "type": "response.completed",
            "response": {"id": "response-7", "status": "completed"},
        }),
    ));
    assert!(arm.turns[0].response_events.is_empty());
}

#[test]
fn incremental_live_api_normalization_matches_retained_analysis() {
    let requests = event_loop_fixture("session", "cache", "response");
    let expected = build_event_loop_trace(&requests);
    let mut arm = LiveApiArm::default();
    for request in &requests {
        assert!(arm.observe(
            "outbound",
            request.phase.as_deref().unwrap(),
            Some(request.request_index),
            &request.payload,
        ));
        for event in &request.response_events {
            arm.observe(
                "inbound",
                request.phase.as_deref().unwrap(),
                Some(request.request_index),
                event,
            );
        }
    }

    assert_eq!(arm.turns.len(), expected.turns.len());
    for (actual, expected) in arm.turns.iter().zip(&expected.turns) {
        assert_eq!(Some(actual.phase.as_str()), expected["phase"].as_str());
        assert_eq!(actual.request, expected["request"]);
        assert_eq!(actual.response.as_ref().unwrap(), &expected["response"]);
        assert!(actual.response_events.is_empty());
    }
}

#[test]
fn progress_explains_model_attempt_failures_and_retries() {
    let failure = serde_json::json!({
        "model_call_index": 6,
        "attempt": 1,
        "max_attempts": 5,
        "failure_phase": "receive",
        "error_class": "receive",
        "retryable": true,
        "billing_uncertain": true,
        "error": "failed to receive a Responses WebSocket frame: connection reset"
    });
    assert_eq!(
        summarize_nanocodex(&AgentEventKind::ModelAttemptFailed, &failure),
        "call 6 · attempt 1 · max 5 · phase receive · class receive · retryable true · billing \
             uncertain true · failed to receive a Responses WebSocket frame: connection reset"
    );

    let retry = serde_json::json!({
        "model_call_index": 6,
        "attempt": 1,
        "next_attempt": 2,
        "max_attempts": 5,
        "failure_phase": "receive",
        "error_class": "receive",
        "delay_ns": 209_556_813_u64,
        "opens_new_socket": true,
        "replay_mode": "full_history",
        "error": "failed to receive a Responses WebSocket frame: connection reset"
    });
    assert_eq!(
        summarize_nanocodex(&AgentEventKind::ModelAttemptRetrying, &retry),
        "call 6 · attempt 1 · next 2 · max 5 · phase receive · class receive · delay 210ms · \
             new socket true · replay full_history · failed to receive a Responses WebSocket \
             frame: connection reset"
    );

    let connection = serde_json::json!({
        "transport": "responses_websocket_v2",
        "attempt": 2,
        "purpose": "reconnect",
        "error": "TLS handshake failed"
    });
    assert_eq!(
        summarize_nanocodex(&AgentEventKind::ModelConnectionFailed, &connection),
        "responses_websocket_v2 · attempt 2 · purpose reconnect · TLS handshake failed"
    );
}

#[tokio::test]
async fn progress_log_retains_interleaved_lanes_in_observation_order() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("progress.jsonl");
    let (progress, recorder) = DiffProgress::start(path.clone(), tokio::time::Instant::now())
        .await
        .unwrap();

    progress.emit("nanocodex", "model.call.started", "call 1");
    progress.observe_codex(&serde_json::json!({
        "type": "item.completed",
        "item": {
            "type": "command_execution",
            "command": "printf hello",
            "exit_code": 0,
            "status": "completed"
        }
    }));
    progress.emit("nanocodex", "tool.call", "exec_command");
    recorder.finish(progress).await.unwrap();

    let records = fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["sequence"], 1);
    assert_eq!(records[0]["arm"], "nanocodex");
    assert_eq!(records[1]["sequence"], 2);
    assert_eq!(records[1]["arm"], "codex");
    assert_eq!(records[1]["kind"], "item.completed");
    assert_eq!(
        records[1]["summary"],
        "command_execution · printf hello · exit 0 · completed"
    );
    assert_eq!(records[2]["sequence"], 3);
    assert_eq!(records[2]["arm"], "nanocodex");
}

#[tokio::test]
async fn progress_log_retains_structured_comparison_identity() {
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("progress.jsonl");
    let (progress, recorder) = DiffProgress::start(path.clone(), tokio::time::Instant::now())
        .await
        .unwrap();
    progress.emit_comparison_started(
        &task,
        DifferentialProfile::new(
            nanocodex_agent::Thinking::High,
            NanocodexToolMode::CodeMode,
            CodexToolMode::CodeModeOnly,
        ),
        4,
        "started",
    );
    recorder.finish(progress).await.unwrap();

    let record: serde_json::Value =
        serde_json::from_str(fs::read_to_string(path).unwrap().trim()).unwrap();
    assert_eq!(record["coordinate"]["task_name"], task.name());
    assert_eq!(
        record["coordinate"]["task_content_digest"],
        task.content_digest()
    );
    assert_eq!(record["coordinate"]["thinking"], "high");
    assert_eq!(record["coordinate"]["nanocodex_tool_mode"], "code_mode");
    assert_eq!(record["coordinate"]["codex_tool_mode"], "code_mode_only");
    assert_eq!(record["coordinate"]["trial"], 4);
}

#[tokio::test]
async fn progress_log_heartbeats_during_quiet_lane_work() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("progress.jsonl");
    let (progress, recorder) = DiffProgress::start_with_heartbeat(
        path.clone(),
        tokio::time::Instant::now(),
        std::time::Duration::from_millis(5),
    )
    .await
    .unwrap();
    progress.emit("nanocodex", "model.call.started", "call 8");
    progress.emit("codex", "item.started", "command_execution · apt-get");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    let heartbeat = loop {
        let contents = fs::read_to_string(&path).unwrap();
        if let Some(record) = contents.lines().find_map(|line| {
            let record = serde_json::from_str::<serde_json::Value>(line).unwrap();
            (record["kind"] == "heartbeat").then_some(record)
        }) {
            break record;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "progress recorder did not flush a heartbeat"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    recorder.finish(progress).await.unwrap();

    assert_eq!(heartbeat["arm"], "runner");
    let summary = heartbeat["summary"].as_str().unwrap();
    assert!(summary.contains("nanocodex: model.call.started (call 8)"));
    assert!(summary.contains("codex: item.started (command_execution · apt-get)"));
}

#[tokio::test]
async fn progress_lane_moves_from_agent_completion_into_verifier_work() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("progress.jsonl");
    let (progress, recorder) = DiffProgress::start(path.clone(), tokio::time::Instant::now())
        .await
        .unwrap();
    progress.emit("nanocodex", "run.completed", "model calls 9");
    progress.observe_evaluator("nanocodex", &EvalEventKind::VerifierStarted);
    progress.observe_evaluator(
        "nanocodex",
        &EvalEventKind::VerifierCompleted(VerifierResult {
            exit_code: 0,
            rewards: [("task_reward".to_owned(), 1.0)].into(),
        }),
    );
    recorder.finish(progress).await.unwrap();

    let records = fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records[1]["kind"], "verifier.started");
    assert_eq!(records[1]["summary"], "canonical verifier");
    assert_eq!(records[2]["kind"], "verifier.completed");
    assert_eq!(records[2]["summary"], "exit 0 · task_reward=1");
}

#[tokio::test]
async fn progress_log_reports_normalized_request_drift_and_response_match_live() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("progress.jsonl");
    let (progress, recorder) = DiffProgress::start(path.clone(), tokio::time::Instant::now())
        .await
        .unwrap();
    let nanocodex_request = serde_json::json!({
        "direction": "outbound",
        "phase": "warmup",
        "model_call_index": null,
        "event": {
            "type": "response.create",
            "client_metadata": {"session_id": "nano"},
            "prompt_cache_key": "nano-cache",
            "input": [{
                "type": "additional_tools",
                "tools": [{"type": "custom", "name": "exec"}]
            }]
        }
    });
    let codex_request = serde_json::json!({
        "direction": "outbound",
        "phase": "warmup",
        "request_index": 3,
        "payload": {
            "encoding": "json",
            "event": {
                "type": "response.create",
                "client_metadata": {"session_id": "codex"},
                "prompt_cache_key": "codex-cache",
                "input": [{
                    "type": "additional_tools",
                    "tools": [{"type": "custom", "name": "wait"}]
                }]
            }
        }
    });
    progress.observe_nanocodex_api(&nanocodex_request);
    progress.observe_api_exchange("codex", &codex_request);
    progress.observe_nanocodex_api(&serde_json::json!({
        "direction": "inbound",
        "phase": "warmup",
        "model_call_index": null,
        "event": {
            "type": "response.completed",
            "response": {"id": "nano-response", "status": "completed"}
        }
    }));
    progress.observe_api_exchange(
        "codex",
        &serde_json::json!({
            "direction": "inbound",
            "phase": "warmup",
            "request_index": 3,
            "payload": {
                "encoding": "json",
                "event": {
                    "type": "response.completed",
                    "response": {"id": "codex-response", "status": "completed"}
                }
            }
        }),
    );
    recorder.finish(progress).await.unwrap();

    let records = fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let request_diff = records
        .iter()
        .find(|record| record["kind"] == "api.request.diff")
        .unwrap();
    assert_eq!(request_diff["arm"], "runner");
    assert!(
        request_diff["summary"]
            .as_str()
            .unwrap()
            .contains("tool_configuration")
    );
    assert!(
        records
            .iter()
            .any(|record| record["kind"] == "api.response.match")
    );
}

#[test]
fn api_capture_retains_auxiliary_requests_but_only_aligns_responses_requests() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("api-exchanges.jsonl");
    let records = [
        serde_json::json!({
            "schema_version": 1,
            "sequence": 1,
            "direction": "outbound",
            "transport": "responses_https",
            "request_index": 1,
            "phase": "unknown",
            "kind": "body",
            "method": "GET",
            "path": "/models",
            "payload_bytes": 0,
            "payload": {"encoding": "utf8", "text": ""}
        }),
        serde_json::json!({
            "schema_version": 1,
            "sequence": 2,
            "direction": "inbound",
            "transport": "responses_https",
            "request_index": 1,
            "phase": "unknown",
            "kind": "response_started",
            "status": 200,
            "payload_bytes": 0,
            "payload": {"encoding": "utf8", "text": ""}
        }),
        serde_json::json!({
            "schema_version": 1,
            "sequence": 3,
            "direction": "inbound",
            "transport": "responses_https",
            "request_index": 1,
            "phase": "unknown",
            "kind": "body_chunk",
            "payload_bytes": 13,
            "payload": {"encoding": "json", "event": {"data": []}}
        }),
        serde_json::json!({
            "schema_version": 1,
            "sequence": 4,
            "direction": "inbound",
            "transport": "responses_https",
            "request_index": 1,
            "phase": "unknown",
            "kind": "response_completed",
            "status": 200,
            "payload_bytes": 0,
            "payload": {"encoding": "utf8", "text": ""}
        }),
        serde_json::json!({
            "schema_version": 1,
            "sequence": 5,
            "direction": "outbound",
            "transport": "responses_websocket",
            "request_index": 2,
            "phase": "generation",
            "kind": "message",
            "payload_bytes": 49,
            "payload": {
                "encoding": "json",
                "event": {"type": "response.create", "model": "gpt-test"}
            }
        }),
        serde_json::json!({
            "schema_version": 1,
            "sequence": 6,
            "direction": "inbound",
            "transport": "responses_websocket",
            "request_index": 2,
            "phase": "generation",
            "kind": "message",
            "payload_bytes": 58,
            "payload": {
                "encoding": "json",
                "event": {
                    "type": "response.completed",
                    "response": {"id": "resp_test"}
                }
            }
        }),
    ];
    let mut jsonl = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    jsonl.push('\n');
    fs::write(&path, jsonl).unwrap();

    let capture =
        inspect_api_exchanges(path.clone(), "all_api_payloads", "exact_wire_payload_bytes")
            .unwrap();
    assert_eq!(capture.summary.requests, 2);
    assert_eq!(capture.summary.response_requests, 1);
    assert_eq!(capture.summary.auxiliary_requests, 1);
    assert_eq!(capture.summary.terminal_events, 1);
    assert_eq!(capture.summary.http_responses_completed, 1);
    assert!(capture.summary.exchange_complete);

    let requests = read_api_request_payloads(&path).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].request_index, 2);
    assert_eq!(requests[0].payload["type"], "response.create");
}

#[test]
fn api_comparison_counts_only_paired_requests_as_aligned_or_differing() {
    let temporary = tempdir().unwrap();
    let nanocodex_path = temporary.path().join("nanocodex.jsonl");
    let codex_path = temporary.path().join("codex.jsonl");
    let report_path = temporary.path().join("comparison.json");
    let request = |request_index| {
        serde_json::json!({
            "schema_version": 1,
            "sequence": request_index,
            "direction": "outbound",
            "transport": "responses_websocket",
            "request_index": request_index,
            "phase": "generation",
            "kind": "message",
            "payload": {
                "encoding": "json",
                "event": {
                    "type": "response.create",
                    "model": "gpt-test",
                    "reasoning": {
                        "effort": "medium",
                        "summary": "auto"
                    },
                    "input": [{
                        "type": "additional_tools",
                        "tools": [
                            {
                                "type": "custom",
                                "name": "exec",
                                "description": "execute code\n\n### `exec_command`\nRun a command.\n\n### `write_stdin`\nWrite input."
                            },
                            {"type": "function", "name": "wait"}
                        ]
                    }]
                }
            }
        })
    };
    fs::write(&nanocodex_path, format!("{}\n", request(1))).unwrap();
    fs::write(&codex_path, format!("{}\n{}\n", request(1), request(2))).unwrap();

    let summary = compare_api_exchanges(
        &report_path,
        Some(&nanocodex_path),
        Some(&codex_path),
        None,
        None,
    )
    .unwrap();

    assert_eq!(summary.aligned_requests, 1);
    assert_eq!(summary.equal_requests, 1);
    assert_eq!(summary.differing_requests, 0);
    assert_eq!(summary.nanocodex_unpaired_requests, 0);
    assert_eq!(summary.codex_unpaired_requests, 1);
    assert_eq!(summary.event_loop.aligned_turns, 1);
    assert_eq!(summary.event_loop.equal_turns, 1);
    assert_eq!(summary.event_loop.differing_turns, 0);
    assert_eq!(summary.event_loop.nanocodex_unpaired_turns, 0);
    assert_eq!(summary.event_loop.codex_unpaired_turns, 1);
    assert_eq!(
        summary.event_loop.nanocodex_unpaired_tail.as_ref().unwrap(),
        &ApiEventLoopTailSummary::default()
    );
    assert_eq!(
        summary.event_loop.codex_unpaired_tail.as_ref().unwrap(),
        &ApiEventLoopTailSummary {
            turns: 1,
            generation_turns: 1,
            turns_without_usage: 1,
            ..ApiEventLoopTailSummary::default()
        }
    );
    assert_eq!(
        summary.event_loop.initial_code_mode_tool_names_equal,
        Some(true)
    );
    assert_eq!(
        summary.event_loop.initial_code_mode_tool_definitions_equal,
        Some(true)
    );
    assert_eq!(
        summary.event_loop.initial_visible_tool_definitions_equal,
        Some(true)
    );
    assert_eq!(
        summary
            .event_loop
            .initial_generation_visible_tool_definitions_equal,
        Some(true)
    );
    assert_eq!(
        summary.event_loop.initial_input_text_sections_equal,
        Some(true)
    );
    assert_eq!(
        summary
            .event_loop
            .initial_generation_input_text_sections_equal,
        Some(true)
    );
    assert_eq!(
        summary
            .event_loop
            .nanocodex
            .as_ref()
            .unwrap()
            .initial_visible_tools,
        ["exec", "wait"]
    );
    assert!(
        validate_differential_profile(
            &summary,
            "gpt-test",
            "medium",
            NanocodexToolMode::CodeModeOnly,
            CodexToolMode::CodeModeOnly,
            false,
        )
        .is_none()
    );
    let mut mismatched_tool_definition = summary.clone();
    mismatched_tool_definition
        .event_loop
        .initial_visible_tool_definitions_equal = Some(false);
    assert!(
        validate_differential_profile(
            &mismatched_tool_definition,
            "gpt-test",
            "medium",
            NanocodexToolMode::CodeModeOnly,
            CodexToolMode::CodeModeOnly,
            false,
        )
        .is_some()
    );
    let mut normal_code_mode = summary.clone();
    normal_code_mode
        .event_loop
        .codex
        .as_mut()
        .unwrap()
        .initial_visible_tools = [
        "exec",
        "wait",
        "exec_command",
        "write_stdin",
        "update_plan",
        "apply_patch",
        "view_image",
        "image_gen",
    ]
    .map(str::to_owned)
    .to_vec();
    normal_code_mode
        .event_loop
        .codex
        .as_mut()
        .unwrap()
        .initial_code_mode_tools = None;
    normal_code_mode
        .event_loop
        .codex
        .as_mut()
        .unwrap()
        .initial_code_mode_tool_definitions = None;
    normal_code_mode
        .event_loop
        .initial_code_mode_tool_names_equal = None;
    normal_code_mode
        .event_loop
        .initial_code_mode_tool_definitions_equal = None;
    assert!(
        validate_differential_profile(
            &normal_code_mode,
            "gpt-test",
            "medium",
            NanocodexToolMode::CodeModeOnly,
            CodexToolMode::CodeMode,
            false,
        )
        .is_none()
    );
    let mut both_normal_code_mode = normal_code_mode.clone();
    both_normal_code_mode
        .event_loop
        .nanocodex
        .as_mut()
        .unwrap()
        .initial_visible_tools = [
        "exec",
        "wait",
        "exec_command",
        "write_stdin",
        "update_plan",
        "apply_patch",
        "view_image",
        "image_gen",
    ]
    .map(str::to_owned)
    .to_vec();
    assert!(
        validate_differential_profile(
            &both_normal_code_mode,
            "gpt-test",
            "medium",
            NanocodexToolMode::CodeMode,
            CodexToolMode::CodeMode,
            false,
        )
        .is_none()
    );
    let mut mismatched_profile = summary.clone();
    mismatched_profile
        .event_loop
        .codex
        .as_mut()
        .unwrap()
        .initial_reasoning_effort = Some("high".to_owned());
    assert!(
        validate_differential_profile(
            &mismatched_profile,
            "gpt-test",
            "medium",
            NanocodexToolMode::CodeModeOnly,
            CodexToolMode::CodeModeOnly,
            false,
        )
        .is_some()
    );
    assert_eq!(
        summary
            .event_loop
            .first_divergence
            .as_ref()
            .map(|divergence| divergence.request_index),
        Some(2)
    );
    assert_eq!(
        summary
            .event_loop
            .first_generation_divergence
            .as_ref()
            .map(|divergence| divergence.request_index),
        Some(2)
    );

    let report: serde_json::Value =
        serde_json::from_reader(fs::File::open(report_path).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 15);
    assert_eq!(report["aligned_requests"], 1);
    assert_eq!(report["codex_unpaired_requests"], 1);
    assert_eq!(report["equal_requests"], 1);
    assert_eq!(report["differing_requests"], 0);
    assert_eq!(report["requests"].as_array().unwrap().len(), 2);
}

#[test]
fn event_loop_normalization_ignores_volatile_identity_but_preserves_links() {
    let left = event_loop_fixture("left-session", "left-cache", "left-response");
    let right = event_loop_fixture("right-session", "right-cache", "right-response");

    let left = build_event_loop_trace(&left);
    let right = build_event_loop_trace(&right);

    assert_eq!(left.turns, right.turns);
    assert_eq!(left.summary.previous_response_links, 1);
    assert_eq!(left.summary.broken_previous_response_links, 0);
    assert_eq!(right.summary.previous_response_links, 1);
    assert_eq!(right.summary.broken_previous_response_links, 0);
    assert_eq!(left.summary.prompt_cache_key_stable, Some(true));
    assert_eq!(right.summary.prompt_cache_key_stable, Some(true));
}

#[test]
fn event_loop_summary_compares_model_visible_tool_sequences() {
    let mut left = event_loop_fixture("left-session", "left-cache", "left-response");
    let mut right = event_loop_fixture("right-session", "right-cache", "right-response");
    for requests in [&mut left, &mut right] {
        requests[1].response_events.insert(
            1,
            serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "call_id": "call-1",
                    "input": "text(await tools.exec_command({cmd: \"true\"}));"
                }
            }),
        );
    }

    let left = build_event_loop_trace(&left);
    let right = build_event_loop_trace(&right);

    assert_eq!(left.summary.model_visible_tool_calls, 1);
    assert_eq!(left.summary.model_visible_tool_sequence, ["exec"]);
    assert_eq!(
        left.summary.model_visible_tool_sequence,
        right.summary.model_visible_tool_sequence
    );
    let mut arm = ArmSummary::runner_error();
    arm.observed_tool_events = Some(7);
    arm.apply_model_visible_tool_calls(Some(&left.summary));
    assert_eq!(arm.tool_calls, Some(1));
    assert_eq!(arm.observed_tool_events, Some(7));
    let mut retained = serde_json::json!({"summary": {"tool_calls": 7}});
    normalize_retained_arm_tool_calls(retained.as_object_mut().unwrap(), Some(&left.summary));
    assert_eq!(retained["summary"]["tool_calls"], 1);
    assert_eq!(retained["summary"]["observed_tool_events"], 7);
    assert_eq!(
        retained["summary"]["tool_call_measurement"],
        super::MODEL_VISIBLE_TOOL_CALL_MEASUREMENT
    );
}

#[test]
fn event_loop_summary_fingerprints_complete_visible_tool_definitions() {
    let left = event_loop_fixture("left-session", "left-cache", "left-response");
    let mut right = event_loop_fixture("right-session", "right-cache", "right-response");
    right[0].payload["input"][0]["tools"][0]["description"] = serde_json::json!(
        "execute code\n\n### `exec_command`\nRun a command in a PTY.\n\n### `write_stdin`\nWrite input."
    );

    let left = build_event_loop_trace(&left);
    let right = build_event_loop_trace(&right);

    assert_eq!(left.summary.initial_visible_tools, ["exec"]);
    assert_eq!(
        left.summary.initial_visible_tools,
        right.summary.initial_visible_tools
    );
    assert_ne!(
        left.summary.initial_visible_tool_definitions,
        right.summary.initial_visible_tool_definitions
    );
    assert_ne!(
        left.summary.initial_visible_tool_definitions[0].description_sha256,
        right.summary.initial_visible_tool_definitions[0].description_sha256
    );
    assert_ne!(
        left.summary.initial_visible_tool_definitions[0].definition_sha256,
        right.summary.initial_visible_tool_definitions[0].definition_sha256
    );
}

#[test]
fn event_loop_summary_extracts_nested_code_mode_tool_catalog_in_order() {
    let mut left = event_loop_fixture("left-session", "left-cache", "left-response");
    let mut right = event_loop_fixture("right-session", "right-cache", "right-response");
    left[0].payload["input"][0]["tools"][0]["description"] = serde_json::json!(
        "execute code\n\n### `exec_command`\nRun a command.\n\n### `view_image`\nView an image."
    );
    right[0].payload["input"][0]["tools"][0]["description"] = serde_json::json!(
        "execute code\n\n### `exec_command`\nRun a command.\n\n### `write_stdin`\nWrite input."
    );

    let left = build_event_loop_trace(&left);
    let right = build_event_loop_trace(&right);

    assert_eq!(
        left.summary.initial_code_mode_tools.as_deref(),
        Some(["exec_command".to_owned(), "view_image".to_owned()].as_slice())
    );
    assert_eq!(
        right.summary.initial_code_mode_tools.as_deref(),
        Some(["exec_command".to_owned(), "write_stdin".to_owned()].as_slice())
    );
    assert_ne!(
        left.summary.initial_code_mode_tools,
        right.summary.initial_code_mode_tools
    );
    let left_definitions = left
        .summary
        .initial_code_mode_tool_definitions
        .as_ref()
        .unwrap();
    let right_definitions = right
        .summary
        .initial_code_mode_tool_definitions
        .as_ref()
        .unwrap();
    assert_eq!(left_definitions[0].name, "exec_command");
    assert_eq!(left_definitions[0].ordinal, 0);
    assert_eq!(left_definitions[0], right_definitions[0]);
    assert_ne!(left_definitions[1], right_definitions[1]);
}

#[test]
fn event_loop_summary_detects_changed_nested_tool_definitions_with_equal_names() {
    let mut left = event_loop_fixture("left-session", "left-cache", "left-response");
    let mut right = event_loop_fixture("right-session", "right-cache", "right-response");
    left[0].payload["input"][0]["tools"][0]["description"] =
        serde_json::json!("execute code\n\n### `exec_command`\nRun a command.");
    right[0].payload["input"][0]["tools"][0]["description"] =
        serde_json::json!("execute code\n\n### `exec_command`\nRun a command in a PTY.");

    let left = build_event_loop_trace(&left);
    let right = build_event_loop_trace(&right);

    assert_eq!(
        left.summary.initial_code_mode_tools,
        right.summary.initial_code_mode_tools
    );
    assert_ne!(
        left.summary.initial_code_mode_tool_definitions,
        right.summary.initial_code_mode_tool_definitions
    );
}

#[test]
fn event_loop_summary_fingerprints_initial_model_input_text() {
    let mut left = event_loop_fixture("left-session", "left-cache", "left-response");
    let mut right = event_loop_fixture("right-session", "right-cache", "right-response");
    for requests in [&mut left, &mut right] {
        requests[0].payload["input"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": "<permissions instructions>\nfull access\n</permissions instructions>"
                }]
            }));
        requests[1].payload["input"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "<environment_context>\n  <shell>bash</shell>\n</environment_context>"
                }]
            }));
    }
    right[1].payload["input"][1]["content"][0]["text"] =
        serde_json::json!("<environment_context>\n  <shell>sh</shell>\n</environment_context>");

    let left = build_event_loop_trace(&left);
    let right = build_event_loop_trace(&right);

    assert_eq!(
        left.summary.initial_input_text_sections,
        right.summary.initial_input_text_sections
    );
    assert_ne!(
        left.summary.initial_generation_input_text_sections,
        right.summary.initial_generation_input_text_sections
    );
    let section = &left.summary.initial_generation_input_text_sections[1];
    assert_eq!(section.role, "user");
    assert_eq!(section.label, "environment_context");
    assert_eq!(section.item_ordinal, 1);
    assert_eq!(section.content_ordinal, 0);
}

#[test]
fn event_loop_normalization_classifies_configuration_and_chain_drift() {
    let left = event_loop_fixture("left-session", "left-cache", "left-response");
    let mut right = event_loop_fixture("right-session", "right-cache", "right-response");
    right[0]
        .payload
        .pointer_mut("/input/0/tools/0/name")
        .unwrap()
        .clone_from(&serde_json::json!("wait"));
    right[0]
        .payload
        .pointer_mut("/reasoning")
        .and_then(serde_json::Value::as_object_mut)
        .unwrap()
        .remove("summary");
    right[1].payload["previous_response_id"] = serde_json::json!("not-the-prior-response");

    let left = build_event_loop_trace(&left);
    let right = build_event_loop_trace(&right);
    let mut differences = Vec::new();
    diff_json(
        "",
        left.turns.first(),
        right.turns.first(),
        &mut differences,
    );

    assert_eq!(
        event_loop_difference_categories(&differences),
        vec![
            "reasoning_policy".to_owned(),
            "tool_configuration".to_owned()
        ]
    );
    assert_eq!(right.summary.previous_response_links, 0);
    assert_eq!(right.summary.broken_previous_response_links, 1);
    assert_eq!(
        right.turns[1]["request"]["previous_response_id"],
        "present_unmatched"
    );
}

#[test]
fn event_loop_recognizes_full_history_replay_and_replayed_tool_results() {
    let mut requests = event_loop_fixture("session", "cache", "response");
    requests[0].response_events.insert(
        1,
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "replayed-call",
                "input": "text(await tools.exec_command({cmd: \"true\"}));"
            }
        }),
    );
    requests[1].response_events.truncate(1);
    requests.push(ApiRequestPayload {
        request_index: 3,
        phase: Some("generation".to_owned()),
        payload: serde_json::json!({
            "type": "response.create",
            "prompt_cache_key": "cache",
            "input": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "I ran a command."}]
                },
                {
                    "type": "reasoning",
                    "encrypted_content": "opaque"
                },
                {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "call_id": "replayed-call",
                    "input": "text(await tools.exec_command({cmd: \"true\"}));"
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "replayed-call",
                    "output": "ok"
                }
            ]
        }),
        sha256: String::new(),
        response_events: vec![serde_json::json!({
            "type": "response.completed",
            "response": {"id": "response-third", "status": "completed"}
        })],
    });

    let trace = build_event_loop_trace(&requests);

    assert_eq!(trace.summary.previous_response_links, 1);
    assert_eq!(trace.summary.full_history_replays, 1);
    assert_eq!(trace.summary.full_history_replays_after_nonterminal_turn, 1);
    assert_eq!(trace.summary.broken_previous_response_links, 0);
    assert_eq!(trace.summary.tool_result_links, 1);
    assert_eq!(trace.summary.replayed_tool_result_links, 1);
    assert_eq!(trace.summary.broken_tool_result_links, 0);
    assert!(
        serde_json::to_string(&trace.turns[2])
            .unwrap()
            .contains("matches_replayed_output")
    );
    assert!(
        !serde_json::to_string(&trace.turns[2])
            .unwrap()
            .contains("present_unmatched")
    );
}

#[test]
fn trajectory_summary_identifies_only_empty_stdin_poll_roundtrips() {
    let steps: Vec<AtifStep> = serde_json::from_value(serde_json::json!([
        {
            "step_id": 1,
            "source": "agent",
            "model_name": "gpt-test",
            "reasoning_effort": "medium",
            "message": "",
            "tool_calls": [
                {
                    "tool_call_id": "wrapper",
                    "function_name": "exec",
                    "arguments": {"raw": "await tools.write_stdin(...)"},
                    "extra": {"model_call_index": 1}
                },
                {
                    "tool_call_id": "poll",
                    "function_name": "write_stdin",
                    "arguments": {
                        "session_id": 2,
                        "chars": "",
                        "yield_time_ms": 1000
                    },
                    "extra": {"model_call_index": 1}
                }
            ],
            "observation": {
                "results": [{
                    "source_call_id": "poll",
                    "content": "still running",
                    "extra": {"status": "completed", "duration_ns": 5000000000_u64}
                }]
            },
            "metrics": {
                "prompt_tokens": 100,
                "completion_tokens": 4,
                "cached_tokens": 80,
                "extra": {
                    "model_call_index": 1,
                    "attempt": 1,
                    "connection_generation": 1,
                    "duration_ns": 3000000000_u64,
                    "time_to_first_event_ns": 1,
                    "time_to_first_output_ns": 2,
                    "tool_calls": 1,
                    "cache_write_input_tokens": 0,
                    "reasoning_output_tokens": 0
                }
            },
            "llm_call_count": 1
        },
        {
            "step_id": 2,
            "source": "agent",
            "message": "",
            "tool_calls": [{
                "tool_call_id": "input",
                "function_name": "write_stdin",
                "arguments": {"session_id": 2, "chars": "q"},
                "extra": {"model_call_index": 2}
            }],
            "llm_call_count": 1
        },
        {
            "step_id": 3,
            "source": "agent",
            "message": "",
            "tool_calls": [
                {
                    "tool_call_id": "poll-and-work",
                    "function_name": "write_stdin",
                    "arguments": {"session_id": 2},
                    "extra": {"model_call_index": 3}
                },
                {
                    "tool_call_id": "work",
                    "function_name": "apply_patch",
                    "arguments": {"patch": "*** Begin Patch"},
                    "extra": {"model_call_index": 3}
                }
            ],
            "llm_call_count": 1
        }
    ]))
    .unwrap();

    let summary = ShellPollingSummary::new(&steps);

    assert_eq!(summary.poll_only_steps, 1);
    assert!(summary.model_call_attribution_complete);
    assert_eq!(summary.confirmed_model_calls, Some(1));
    assert_eq!(summary.empty_stdin_tool_calls, 1);
    assert_eq!(summary.sessions, 1);
    assert_eq!(summary.explicit_requested_yield_ms, 1_000);
    assert_eq!(summary.tool_wait_duration_ns, 5_000_000_000);
    assert_eq!(summary.model_duration_ns, 3_000_000_000);
    assert_eq!(summary.prompt_tokens, 100);
    assert_eq!(summary.cached_tokens, 80);
    assert_eq!(summary.completion_tokens, 4);
}

#[test]
fn raw_api_summary_detects_poll_only_model_responses_and_usage() {
    let events = vec![
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "call-1",
                "input": "const r = await tools.write_stdin({ session_id: 2, chars: \"\", yield_time_ms: 1000 }); text(r.output);"
            }
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "usage": {
                    "input_tokens": 100,
                    "input_tokens_details": {"cached_tokens": 80},
                    "output_tokens": 4
                }
            }
        }),
    ];

    let polling = detected_polling_turn(&events).unwrap();

    assert_eq!(polling.empty_stdin_calls, 1);
    assert_eq!(polling.calls_with_explicit_yield, 1);
    assert_eq!(polling.explicit_requested_yield_ms, 1_000);
    assert_eq!(polling.input_tokens, 100);
    assert_eq!(polling.cached_tokens, 80);
    assert_eq!(polling.output_tokens, 4);
    let direct = detected_polling_turn(&[serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "name": "write_stdin",
            "call_id": "call-2",
            "arguments": "{\"session_id\":2,\"yield_time_ms\":30000}"
        }
    })])
    .unwrap();
    assert_eq!(direct.empty_stdin_calls, 1);
    assert_eq!(direct.calls_with_explicit_yield, 1);
    assert_eq!(direct.explicit_requested_yield_ms, 30_000);
    assert_eq!(
        detected_code_mode_empty_stdin_calls(concat!(
            "await tools.write_stdin({session_id: 2, chars: \"\", yield",
            "_",
            "time_ms: 1000});"
        )),
        Some(DetectedEmptyStdinCalls {
            calls: 1,
            calls_with_explicit_yield: 1,
            explicit_requested_yield_ms: 1_000,
        })
    );
    assert_eq!(
        detected_code_mode_empty_stdin_calls(concat!(
            "await tools.write_stdin({session_id: 2}); await tools.write_stdin({session_id: 3, \"yield",
            "_",
            "time_ms\": 30000});"
        )),
        Some(DetectedEmptyStdinCalls {
            calls: 2,
            calls_with_explicit_yield: 1,
            explicit_requested_yield_ms: 30_000,
        })
    );
    assert_eq!(
        detected_code_mode_empty_stdin_calls(
            "await tools.write_stdin({session_id: 2, chars: \"q\"});"
        ),
        None
    );
    assert_eq!(
        detected_code_mode_empty_stdin_calls(
            "await tools.write_stdin({session_id: 2}); await tools.exec_command({cmd: \"pwd\"});"
        ),
        None
    );
}

#[test]
fn event_loop_unpaired_tail_aggregates_cache_and_poll_cost() {
    let completed = |input, cached, output, reasoning, total| {
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": format!("response-{total}"),
                "status": "completed",
                "usage": {
                    "input_tokens": input,
                    "input_tokens_details": {"cached_tokens": cached},
                    "output_tokens": output,
                    "output_tokens_details": {"reasoning_tokens": reasoning},
                    "total_tokens": total
                }
            }
        })
    };
    let request = |request_index, response_events| ApiRequestPayload {
        request_index,
        phase: Some("generation".to_owned()),
        payload: serde_json::json!({
            "type": "response.create",
            "model": "gpt-test",
            "generate": true,
            "input": []
        }),
        sha256: format!("request-{request_index}"),
        response_events,
    };
    let requests = vec![
        request(1, vec![completed(10, 0, 3, 1, 13)]),
        request(
            2,
            vec![
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "custom_tool_call",
                        "name": "exec",
                        "call_id": "poll",
                        "input": "await tools.write_stdin({session_id: 2});"
                    }
                }),
                completed(100, 80, 10, 4, 110),
            ],
        ),
        request(3, vec![completed(120, 100, 5, 2, 125)]),
    ];

    let trace = build_event_loop_trace(&requests);

    assert_eq!(trace.summary.turns_with_usage, 3);
    assert_eq!(trace.summary.turns_without_usage, 0);
    assert_eq!(
        trace.summary.usage,
        ApiTokenUsageSummary {
            input_tokens: 230,
            cached_input_tokens: 180,
            uncached_input_tokens: 50,
            output_tokens: 18,
            reasoning_output_tokens: 7,
            total_tokens: 248,
        }
    );

    let tail = trace.unpaired_tail(1);

    assert_eq!(
        tail,
        ApiEventLoopTailSummary {
            turns: 2,
            generation_turns: 2,
            tool_call_turns: 1,
            detected_poll_only_turns: 1,
            detected_empty_stdin_calls: 1,
            detected_polling_calls_with_explicit_yield: 0,
            detected_polling_explicit_yield_ms: 0,
            turns_with_usage: 2,
            turns_without_usage: 0,
            usage: ApiTokenUsageSummary {
                input_tokens: 220,
                cached_input_tokens: 180,
                uncached_input_tokens: 40,
                output_tokens: 15,
                reasoning_output_tokens: 6,
                total_tokens: 235,
            },
        }
    );
}

#[tokio::test]
async fn codex_arm_uses_the_native_workspace_verifier_and_event_lifecycle() {
    let temporary = tempdir().unwrap();
    let binary = write_fake_codex(temporary.path());
    let codex = CodexExec::new(&binary, MODEL, "medium")
        .unwrap()
        .api_key("test");
    let task =
        Task::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"))
            .unwrap();
    let agent = Nanocodex::builder(OpenAi::new("unused").unwrap());
    let configured = codex.clone();
    let report = run_arm(
        task,
        Evaluator::new_builder(agent)
            .output_directory(temporary.path().join("evaluations"))
            .attempt_agent(move |_attempt, _builder| {
                Ok::<_, Infallible>(AttemptAgent::codex(configured.clone()))
            }),
        TrajectoryProjection::Codex {
            version: CodexVersion::Fixed("codex-cli-test".to_owned()),
        },
        false,
        DiffProgress::default(),
    )
    .await;

    assert!(report.operational_error.is_none());
    assert!(report.event_error.is_none());
    assert!(report.trajectory_error.is_none());
    assert!(matches!(report.summary.status, ArmStatus::Passed));
    assert_eq!(report.summary.tool_calls, None);
    assert_eq!(report.summary.observed_tool_events, Some(1));
    assert_eq!(
        report
            .summary
            .usage
            .as_ref()
            .map(|usage| usage.total_tokens),
        Some(17)
    );
    assert!(
        report
            .codex_events
            .as_ref()
            .is_some_and(|path| path.is_file())
    );
    assert!(
        report
            .codex_stderr
            .as_ref()
            .is_some_and(|path| path.is_file())
    );
    assert!(
        report
            .codex_summary
            .as_ref()
            .is_some_and(|path| path.is_file())
    );
    assert!(
        report
            .trajectory
            .as_ref()
            .is_some_and(|path| path.is_file())
    );
    let trajectory: AtifTrajectory =
        serde_json::from_slice(&fs::read(report.trajectory.as_ref().unwrap()).unwrap()).unwrap();
    assert_eq!(trajectory.agent.name, "codex");
    assert_eq!(trajectory.agent.version, "codex-cli-test");
    assert_eq!(trajectory.tool_call_count(), 1);
    assert_eq!(trajectory.observation_count(), 1);
    assert_eq!(
        report
            .trajectory_summary
            .as_ref()
            .and_then(|summary| summary.model_calls),
        None
    );
    assert!(report.event_log.as_ref().is_some_and(|path| path.is_file()));
    let event_log = fs::read_to_string(report.event_log.as_ref().unwrap()).unwrap();
    assert!(event_log.contains("\"type\":\"attempt_started\""));
    assert!(event_log.contains("\"type\":\"verifier_completed\""));
    assert!(event_log.contains("\"type\":\"completed\""));

    let EvalAttemptOutcome::Scored(outcome) = report.outcome.unwrap() else {
        panic!("fake Codex attempt should be scored");
    };
    assert_eq!(outcome.status, EvalStatus::Passed);
    let agent = outcome.agent.unwrap();
    assert_eq!(agent.metadata.status, AgentStatus::Completed);
    assert_eq!(agent.metadata.transport, "codex_exec_jsonl");
    assert_eq!(agent.metadata.orchestration, "stock_codex_cli");
    assert_eq!(
        fs::read_to_string(outcome.artifacts.workspace.join("greeting.txt")).unwrap(),
        "hello from nanoeval\n"
    );
}

fn write_fake_codex(directory: &Path) -> PathBuf {
    let binary = directory.join("codex");
    fs::write(
            &binary,
            r#"#!/bin/sh
set -eu
if [ "${1:-}" = "--version" ]; then
  printf '%s\n' 'codex-cli-test'
  exit 0
fi
printf '%s\n' '{"type":"thread.started","thread_id":"00000000-0000-0000-0000-000000000001"}'
printf '%s\n' '{"type":"turn.started"}'
printf '%s\n' '{"type":"item.completed","item":{"id":"item-1","type":"command_execution","command":"printf greeting","aggregated_output":"","exit_code":0,"status":"completed"}}'
printf 'hello from nanoeval\n' > greeting.txt
printf '%s\n' '{"type":"item.completed","item":{"id":"item-2","type":"agent_message","text":"done"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":4,"output_tokens":7}}'
printf '%s\n' 'fake diagnostic' >&2
"#,
        )
        .unwrap();
    let mut permissions = fs::metadata(&binary).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&binary, permissions).unwrap();
    binary
}

fn event_loop_fixture(
    session_id: &str,
    prompt_cache_key: &str,
    first_response_id: &str,
) -> Vec<ApiRequestPayload> {
    let second_response_id = format!("{first_response_id}-second");
    vec![
        ApiRequestPayload {
            request_index: 1,
            phase: Some("warmup".to_owned()),
            payload: serde_json::json!({
                "type": "response.create",
                "generate": false,
                "prompt_cache_key": prompt_cache_key,
                "client_metadata": {
                    "session_id": session_id,
                    "thread_id": format!("{session_id}-thread"),
                    "ws_request_header_x_openai_internal_codex_responses_lite": "true"
                },
                "reasoning": {
                    "context": "all_turns",
                    "effort": "medium",
                    "summary": "auto"
                },
                "input": [{
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{
                        "type": "custom",
                        "name": "exec",
                        "description": "execute code\n\n### `exec_command`\nRun a command.\n\n### `write_stdin`\nWrite input."
                    }]
                }]
            }),
            sha256: String::new(),
            response_events: vec![
                serde_json::json!({
                    "type": "response.created",
                    "response": {"id": first_response_id, "status": "in_progress"}
                }),
                serde_json::json!({
                    "type": "response.completed",
                    "response": {"id": first_response_id, "status": "completed"}
                }),
            ],
        },
        ApiRequestPayload {
            request_index: 2,
            phase: Some("generation".to_owned()),
            payload: serde_json::json!({
                "type": "response.create",
                "prompt_cache_key": prompt_cache_key,
                "previous_response_id": first_response_id,
                "client_metadata": {
                    "session_id": session_id,
                    "thread_id": format!("{session_id}-thread"),
                    "turn_id": format!("{session_id}-turn"),
                    "ws_request_header_x_openai_internal_codex_responses_lite": "true"
                },
                "reasoning": {
                    "context": "all_turns",
                    "effort": "medium",
                    "summary": "auto"
                },
                "input": [{
                    "id": format!("{session_id}-message"),
                    "type": "message",
                    "role": "user",
                    "internal_chat_message_metadata_passthrough": {
                        "turn_id": format!("{session_id}-turn")
                    },
                    "content": [{"type": "input_text", "text": "same prompt"}]
                }]
            }),
            sha256: String::new(),
            response_events: vec![
                serde_json::json!({
                    "type": "response.created",
                    "response": {"id": second_response_id, "status": "in_progress"}
                }),
                serde_json::json!({
                    "type": "response.completed",
                    "response": {"id": second_response_id, "status": "completed"}
                }),
            ],
        },
    ]
}
