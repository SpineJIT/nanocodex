use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fs,
    future::Future,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use arcbox_ext4::Reader;
use chrono::{DateTime, Utc};
use clap::{Args, ValueEnum};
use eyre::{Result, eyre};
use nanocodex::*;
use nanocodex_eval::{aggregate::*, harbor::*, vm::*, *};
use regex::RegexSet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sysinfo::{MemoryRefreshKind, RefreshKind, System};
use tokio::{process::Command, sync::watch};
use tracing::{info, warn};
use yansi::Painted;

use crate::{config::EvalAgentArgs, observability::ObservabilityArgs};

use super::args::{SchedulingArgs, VmPreparationArgs};

mod measurements;
mod report;
mod retained;
mod runtime;

use measurements::RunMeasurements;
#[cfg(test)]
use report::RunSummary;
use report::{AttemptOutcome, Progress, RunReport, report_progress, scored_results};
use retained::{
    load_invocation, load_required_invocation, persist_invocation, record_last_run,
    resolve_rerun_source, write_json_atomic, write_task_names,
};
#[cfg(test)]
use retained::{retained_retry_task_names, retry_matcher};
#[cfg(test)]
use runtime::{
    GUEST_RUNTIME_ARTIFACT_ROOT, GUEST_RUNTIME_CACHE_ROOT, VM_GUEST_ELF_MACHINE, VM_GUEST_TARGET,
    ensure_job_owned_path, parse_cargo_dep_info, prepare_job_guest_runtime_disk,
    prepare_retained_guest_runtime, retain_guest_runtime_bytes, validate_vm_guest_commit,
    vm_guest_build_command, vm_guest_runtime_is_fresh, write_vm_guest_build_record,
};
use runtime::{prepare_runtime_for_vm, stable_file_sha256};
pub(crate) use runtime::{prepare_vm_guest_runtime, prepare_vm_guest_runtime_from};

const DEFAULT_OUTPUT_DIRECTORY: &str = ".nanocodex/evals";
const INVOCATION_FILE: &str = "invocation.json";
const LAST_RUN_FILE: &str = ".nanocodex/eval/last-run.json";
const INVOCATION_VERSION: u32 = 3;
const SCHEDULING_POLICY: &str = "bounded_fifo_work_conserving-v1";
const TARGET_EVAL_OPEN_FILES: u64 = 8_192;
const BYTES_PER_MIB: u64 = 1024 * 1024;

#[derive(Args)]
#[group(id = "task_input", required = true, multiple = true)]
pub(crate) struct Run {
    #[command(flatten)]
    observability: ObservabilityArgs,

    /// Terminal-Bench task directory. Repeat for multiple evals in one job.
    #[arg(long = "task", value_name = "DIRECTORY", group = "task_input")]
    tasks: Vec<PathBuf>,

    /// Terminal-Bench suite directory whose immediate task children should run.
    #[arg(long = "suite", value_name = "DIRECTORY", group = "task_input")]
    suites: Vec<PathBuf>,

    #[command(flatten)]
    retry: RetryArgs,

    /// Parent directory for the retained Harbor-compatible job.
    #[arg(long)]
    output: Option<PathBuf>,

    #[command(flatten)]
    scheduling: SchedulingArgs,

    #[command(flatten)]
    lifecycle: RunLifecycleArgs,

    /// Print typed results as JSON instead of a human summary.
    #[arg(long)]
    json: bool,

    /// Override the prepared task rootfs directory or raw ext4 image.
    #[arg(long, value_name = "PATH")]
    vm_rootfs: Option<PathBuf>,

    #[command(flatten)]
    vm: VmPreparationArgs,

    /// Writable VM root-disk retention policy.
    #[arg(long, value_enum)]
    vm_retention: Option<VmRetention>,

    #[command(flatten)]
    agent: EvalAgentArgs,
}

#[derive(Args)]
struct RetryArgs {
    /// Rerun unresolved tasks from the latest completed Evaluator job.
    #[arg(long, group = "task_input", conflicts_with_all = ["tasks", "suites"])]
    rerun: bool,

    /// Literal task-name substring to rerun. Repeat positional values for OR matching.
    #[arg(value_name = "NAME", requires = "rerun")]
    names: Vec<String>,

    /// Resolve the retry queue from this job instead of the latest completed job.
    #[arg(long, value_name = "JOB", requires = "rerun")]
    rerun_from: Option<PathBuf>,

    /// Advanced regular expression over full task names. Repeat for OR matching.
    #[arg(long, value_name = "REGEX", requires = "rerun")]
    match_task: Vec<String>,

    /// Print the selected task names without starting a new evaluation job.
    #[arg(long, requires = "rerun")]
    list: bool,

    #[command(flatten)]
    statuses: RetryStatusArgs,
}

#[derive(Args)]
struct RetryStatusArgs {
    /// Include typed safety refusals in the rerun selection.
    #[arg(long, requires = "rerun")]
    include_refused: bool,

    /// Include harness-errored tasks in the rerun selection.
    #[arg(long, requires = "rerun")]
    include_errored: bool,
}

#[derive(Args)]
struct RunLifecycleArgs {
    /// Start a new job even when a matching incomplete job can be resumed.
    #[arg(long = "new")]
    new_job: bool,
}

#[derive(Clone, Debug)]
struct ResolvedRun {
    task_paths: Vec<PathBuf>,
    output: PathBuf,
    trials: u16,
    concurrency: u16,
    max_memory_mb: Option<u64>,
    vm_rootfs: Option<PathBuf>,
    vm_guest_runtime: Option<PathBuf>,
    vm_cache: PathBuf,
    vm_retention: VmRetention,
    thinking: Thinking,
    web_search: bool,
    rerun_from: Option<PathBuf>,
    automatic_scheduling: Option<AutomaticScheduling>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HostResources {
    logical_cpus: usize,
    physical_memory_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SchedulingDefaults {
    concurrency: u16,
    max_memory_mb: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedScheduling {
    concurrency: u16,
    max_memory_mb: Option<u64>,
    automatic: Option<AutomaticScheduling>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AutomaticScheduling {
    utilization_percent: u8,
    host: HostResources,
    concurrency: bool,
    memory: bool,
}

struct RerunSelection {
    job: PathBuf,
    tasks: Vec<PathBuf>,
}

struct RetainedRetryQueue {
    task_names: BTreeSet<String>,
    unresolved_tasks: usize,
    lineage: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RunInvocation {
    version: u32,
    nanocodex_build: RetainedBuild,
    model: String,
    tool_profile: String,
    seed: Option<u64>,
    scheduling: RetainedScheduling,
    trials: u16,
    concurrency: u16,
    max_memory_mb: Option<u64>,
    vm_rootfs: Option<PathBuf>,
    guest_runtime: Option<RetainedGuestRuntime>,
    vm_retention: VmRetention,
    thinking: String,
    web_search: bool,
    rerun_from: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedBuild {
    version: String,
    git_sha: String,
    built_at: String,
    executable_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedGuestRuntime {
    target: String,
    binary_sha256: String,
    runtime_disk_digest: Option<String>,
    artifact_path: Option<PathBuf>,
    source: String,
    source_path: PathBuf,
    host_git_sha: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedScheduling {
    policy: String,
    automatic_utilization_percent: Option<u8>,
    concurrency_source: String,
    memory_source: String,
}

impl RunInvocation {
    fn same_workload(&self, other: &Self) -> bool {
        self.version == other.version
            && self.nanocodex_build == other.nanocodex_build
            && self.model == other.model
            && self.tool_profile == other.tool_profile
            && self.seed == other.seed
            && self.scheduling.policy == other.scheduling.policy
            && self.trials == other.trials
            && self.vm_rootfs == other.vm_rootfs
            && same_guest_runtime(self.guest_runtime.as_ref(), other.guest_runtime.as_ref())
            && self.vm_retention == other.vm_retention
            && self.thinking == other.thinking
            && self.web_search == other.web_search
            && self.rerun_from == other.rerun_from
    }
}

fn same_guest_runtime(
    left: Option<&RetainedGuestRuntime>,
    right: Option<&RetainedGuestRuntime>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.target == right.target && left.binary_sha256 == right.binary_sha256
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LastRun {
    job: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RetainedRun {
    tasks: Vec<RetainedRunTask>,
}

#[derive(Debug, Deserialize)]
struct RetainedRunTask {
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RetainedJobIdentity {
    started_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct RetainedTrialResult {
    task_name: String,
    outcome: EvalOutcome,
    scored: bool,
    verifier_result: Option<RetainedVerifierResult>,
    exception_info: Option<RetainedTrialException>,
}

#[derive(Debug, Deserialize)]
struct RetainedVerifierResult {
    rewards: BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct RetainedTrialException {
    exception_type: String,
}

impl HostResources {
    fn detect() -> Self {
        let logical_cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        let physical_memory_bytes = match system.total_memory() {
            0 => None,
            bytes => Some(bytes),
        };
        Self {
            logical_cpus,
            physical_memory_bytes,
        }
    }

    fn scheduling_defaults(self, utilization_percent: u8) -> SchedulingDefaults {
        let logical_cpus = u64::try_from(self.logical_cpus).unwrap_or(u64::MAX);
        let concurrency = percentage(logical_cpus, utilization_percent).max(1);
        let concurrency = u16::try_from(concurrency).unwrap_or(u16::MAX);
        let max_memory_mb = self
            .physical_memory_bytes
            .map(|bytes| (percentage(bytes, utilization_percent) / BYTES_PER_MIB).max(1));
        SchedulingDefaults {
            concurrency,
            max_memory_mb,
        }
    }
}

pub(super) fn automatic_scheduling_defaults(utilization_percent: u8) -> (u16, Option<u64>) {
    let defaults = HostResources::detect().scheduling_defaults(utilization_percent);
    (defaults.concurrency, defaults.max_memory_mb)
}

pub(super) fn raise_eval_open_file_limit() -> Result<()> {
    #[cfg(unix)]
    {
        use nix::sys::resource::{Resource, getrlimit, setrlimit};

        let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE)
            .map_err(|error| eyre!("failed to read eval runner open-file limit: {error}"))?;
        let target = desired_eval_open_file_limit(soft, hard);
        if target > soft {
            setrlimit(Resource::RLIMIT_NOFILE, target, hard).map_err(|error| {
                eyre!(
                    "failed to raise eval runner open-file limit from {soft} to {target}: {error}"
                )
            })?;
            eprintln!("Raised eval runner open-file limit from {soft} to {target}");
        }
    }
    Ok(())
}

fn desired_eval_open_file_limit(soft: u64, hard: u64) -> u64 {
    soft.max(hard.min(TARGET_EVAL_OPEN_FILES))
}

const fn percentage(value: u64, percent: u8) -> u64 {
    value.saturating_mul(percent as u64) / 100
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RetainedTrialStatus {
    passed: bool,
    failed: bool,
    refused: bool,
    errored: bool,
}

impl Run {
    fn resolve_scheduling(&self, retained: Option<&RunInvocation>) -> ResolvedScheduling {
        let host = HostResources::detect();
        let defaults = host.scheduling_defaults(self.scheduling.host_utilization);
        let retained_concurrency = retained.map(|invocation| invocation.concurrency);
        let automatic_concurrency =
            self.scheduling.concurrency.is_none() && retained_concurrency.is_none();
        let concurrency = self
            .scheduling
            .concurrency
            .or(retained_concurrency)
            .unwrap_or(defaults.concurrency);
        let (max_memory_mb, automatic_memory) = if let Some(memory) = self.scheduling.max_memory_mb
        {
            (Some(memory), false)
        } else if let Some(invocation) = retained {
            (invocation.max_memory_mb, false)
        } else {
            (defaults.max_memory_mb, defaults.max_memory_mb.is_some())
        };
        let automatic =
            (automatic_concurrency || automatic_memory).then_some(AutomaticScheduling {
                utilization_percent: self.scheduling.host_utilization,
                host,
                concurrency: automatic_concurrency,
                memory: automatic_memory,
            });
        ResolvedScheduling {
            concurrency,
            max_memory_mb,
            automatic,
        }
    }

    fn resolve_run(&self) -> Result<ResolvedRun> {
        if self.vm.vm_refresh && self.vm_rootfs.is_some() {
            return Err(eyre!("--vm-refresh conflicts with --vm-rootfs"));
        }
        let rerun = self
            .retry
            .rerun
            .then(|| resolve_rerun_source(self))
            .transpose()?;
        let retained_invocation = match &rerun {
            Some(rerun) => Some(load_required_invocation(&rerun.job)?),
            None => None,
        };
        let task_paths = match &rerun {
            Some(rerun) => rerun.tasks.clone(),
            None => load_task_paths(self.tasks.clone(), self.suites.clone())?,
        };
        let output = self.output.clone().unwrap_or_else(|| {
            rerun
                .as_ref()
                .and_then(|rerun| rerun.job.parent())
                .map_or_else(
                    || PathBuf::from(DEFAULT_OUTPUT_DIRECTORY),
                    Path::to_path_buf,
                )
        });
        let retained_thinking = retained_invocation
            .as_ref()
            .map(|invocation| {
                Thinking::from_str(&invocation.thinking).map_err(|error| {
                    eyre!(
                        "invalid thinking level {:?} in {INVOCATION_FILE}: {error}",
                        invocation.thinking
                    )
                })
            })
            .transpose()?;
        let thinking = self
            .agent
            .thinking()
            .or(retained_thinking)
            .unwrap_or_default();
        let web_search = self
            .agent
            .web_search()
            .or_else(|| {
                retained_invocation
                    .as_ref()
                    .map(|invocation| invocation.web_search)
            })
            .unwrap_or(false);
        let vm_rootfs = self.vm_rootfs.clone().or_else(|| {
            retained_invocation
                .as_ref()
                .and_then(|invocation| invocation.vm_rootfs.clone())
        });
        let vm_guest_runtime = self
            .vm
            .vm_guest_runtime
            .clone()
            .or_else(|| std::env::var_os("NANOCODEX_VM_GUEST_RUNTIME").map(PathBuf::from));
        let scheduling = self.resolve_scheduling(retained_invocation.as_ref());
        Ok(ResolvedRun {
            task_paths,
            output,
            trials: self.scheduling.trials,
            concurrency: scheduling.concurrency,
            max_memory_mb: scheduling.max_memory_mb,
            vm_rootfs,
            vm_guest_runtime,
            vm_cache: self.vm.vm_cache.clone(),
            vm_retention: self
                .vm_retention
                .or_else(|| {
                    retained_invocation
                        .as_ref()
                        .map(|invocation| invocation.vm_retention)
                })
                .unwrap_or_default(),
            thinking,
            web_search,
            rerun_from: rerun.map(|rerun| rerun.job),
            automatic_scheduling: scheduling.automatic,
        })
    }

    fn resolve_executable_run(&self) -> Result<Option<ResolvedRun>> {
        let resolved = self.resolve_run()?;
        if self.retry.list {
            write_task_names(&resolved.task_paths, self.json)?;
            return Ok(None);
        }
        resolved.report_automatic_scheduling();
        resolved.report_configuration();
        Ok(Some(resolved))
    }

    pub(crate) async fn run(self) -> Result<()> {
        let total_started = Instant::now();
        let Some(resolved) = self.resolve_executable_run()? else {
            return Ok(());
        };
        raise_eval_open_file_limit()?;
        let observability_started = Instant::now();
        let _observability = self.observability.install(false, Path::new("."))?;
        let observability = observability_started.elapsed();
        let (tasks, task_loading) =
            load_prioritized_tasks(resolved.task_paths.clone(), &resolved.output)?;
        let evaluation_setup_started = Instant::now();
        let new_job = self.lifecycle.new_job;
        let nanocodex = self.agent.builder(resolved.thinking, resolved.web_search)?;
        let vm_backend = VmBackend::builder()
            .retain_passed_rootfs(resolved.vm_retention.retains_passes())
            .web_search(resolved.web_search)
            .build();
        let (eval, attempt_count) = Self::build_evaluator(
            &resolved,
            tasks.clone(),
            nanocodex,
            vm_backend.clone(),
            new_job,
        )?;
        let mut evaluation_setup = evaluation_setup_started.elapsed();
        let remaining_attempts = eval.remaining_attempts()?;
        let skipped_attempts = attempt_count.saturating_sub(remaining_attempts);
        let (vmm, runtime_image, guest_runtime, vm_runtime) = prepare_run_vm(
            resolved.vm_rootfs.as_deref(),
            resolved.vm_guest_runtime.as_deref(),
            eval.directory(),
            eval.resumed(),
            resolved.rerun_from.as_deref(),
            eval.resumed() && skipped_attempts == 0,
        )
        .await?;
        let invocation_started = Instant::now();
        persist_invocation(
            eval.directory(),
            &resolved.invocation(guest_runtime.clone())?,
        )?;
        evaluation_setup += invocation_started.elapsed();
        let vm_environments_started = Instant::now();
        let mut resources = VmResources::builder(vmm, runtime_image)
            .tasks(tasks.clone())
            .cache_directory(&resolved.vm_cache)
            .cache_policy(if self.vm.vm_refresh {
                CachePolicy::Refresh
            } else {
                CachePolicy::Reuse
            });
        if let Some(rootfs) = &resolved.vm_rootfs {
            resources = resources.rootfs(rootfs);
        }
        resources.prepare().await?.configure(&vm_backend).await?;
        let vm_environments_duration = vm_environments_started.elapsed();
        report_resume(&eval, skipped_attempts, attempt_count);
        let eval_run = eval.sweep();
        let events = eval_run.events();
        let harbor = Harbor::new(&eval)?.record(events.subscribe())?;
        let (expected_attempts, expected_attempts_rx) = watch::channel(remaining_attempts);
        let progress = tokio::spawn(report_progress(
            events.subscribe(),
            expected_attempts_rx,
            usize::from(resolved.concurrency),
            resolved.max_memory_mb,
        ));
        let attempts_started = Instant::now();
        let interrupts = ctrl_c_interrupt()?;
        let execution = finish_or_drain(eval_run, interrupts, remaining_attempts, || {
            let admitted = eval.begin_drain();
            eprintln!(
                "Interrupt received; stopped admitting new trials after {admitted} \
                     attempt(s), draining admitted work; press Ctrl-C again to abort"
            );
            admitted
        })
        .await?;
        let DrainExecution {
            result: sweep_result,
            terminal_attempts,
            interrupted,
            interrupt,
        } = execution;
        expected_attempts.send_replace(terminal_attempts);
        drop(expected_attempts);
        let attempts = attempts_started.elapsed();
        finish_or_interrupt(
            async move {
                let finished =
                    finish_evaluation(harbor, terminal_attempts, progress, sweep_result).await?;
                tokio::task::yield_now().await;
                let output_started = Instant::now();
                persist_aggregate(&finished.job, vm_environments_duration)?;
                tokio::task::yield_now().await;
                Self::write_report(
                    &finished.job,
                    finished.outcomes,
                    skipped_attempts,
                    vm_environments_duration,
                    self.json,
                )?;
                tokio::task::yield_now().await;
                let output = output_started.elapsed();
                let measurements = RunMeasurements {
                    observability,
                    task_loading,
                    vm_runtime,
                    vm_environments: vm_environments_duration,
                    evaluation_setup,
                    attempts,
                    harbor_finish: finished.harbor_finish,
                    output,
                    total: total_started.elapsed(),
                };
                measurements.persist(finished.job.directory())?;
                measurements.record(&finished.results, attempt_count, finished.failed);
                if !interrupted {
                    record_last_run(finished.job.directory())?;
                }
                finish_run(finished.run_error)?;
                if interrupted {
                    return Err(eyre!(
                        "evaluation interrupted after draining admitted attempts; rerun the same \
                         workload to resume {}",
                        finished.job.directory().display()
                    ));
                }
                Ok(())
            },
            interrupt,
        )
        .await??;
        Ok(())
    }

    fn build_evaluator(
        resolved: &ResolvedRun,
        tasks: Vec<Task>,
        nanocodex: NanocodexBuilder,
        vm_backend: VmBackend,
        new_job: bool,
    ) -> Result<(Evaluator, usize)> {
        let sweep = Sweep::builder()
            .tasks(tasks)
            .trials(resolved.trials)
            .agent("default", nanocodex.clone())?
            .build()?;
        let attempt_count = sweep.attempt_count();
        let evaluator = Evaluator::builder(nanocodex, vm_backend)
            .output_directory(&resolved.output)
            .max_concurrency(usize::from(resolved.concurrency));
        let evaluator = configure_memory_limit(evaluator, resolved.max_memory_mb);
        let evaluator = bind_finite_run(evaluator, sweep, new_job).build()?;
        Ok((evaluator, attempt_count))
    }

    fn write_report(
        job: &HarborJob,
        outcomes: Vec<AttemptOutcome>,
        skipped: usize,
        cold_image_and_cache: Duration,
        json: bool,
    ) -> Result<()> {
        let report = RunReport::new(job, outcomes, skipped, cold_image_and_cache);
        if json {
            serde_json::to_writer_pretty(io::stdout().lock(), &report)?;
            println!();
        } else {
            Self::write_summary(&report);
        }
        Ok(())
    }

    fn write_summary(report: &RunReport) {
        println!(
            "\nScores: {} passed; {} failed; {} scored; {} unscored; {} total",
            Painted::new(report.summary.passed).green(),
            Painted::new(report.summary.failed).red(),
            report.summary.scored,
            report.summary.unscored,
            report.summary.total
        );
        println!(
            "Lifecycle: {} errored attempt{} (including {} safety refusal{})",
            Painted::new(report.summary.errored).red(),
            if report.summary.errored == 1 { "" } else { "s" },
            Painted::new(report.summary.refused).yellow(),
            if report.summary.refused == 1 { "" } else { "s" },
        );
        if report.summary.cleanup_failed > 0 {
            println!(
                "Cleanup health: {} attempt{} failed explicit cleanup",
                Painted::new(report.summary.cleanup_failed).red(),
                if report.summary.cleanup_failed == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        if report.summary.billing_unknown > 0 {
            println!(
                "Billing coverage: {} attempt{} unknown and excluded from exact cost",
                Painted::new(report.summary.billing_unknown).yellow(),
                if report.summary.billing_unknown == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        if report.summary.billing_missing > 0 {
            println!(
                "Billing coverage: {} attempt{} did not retain a billing snapshot",
                Painted::new(report.summary.billing_missing).yellow(),
                if report.summary.billing_missing == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        println!("Harbor job: {}", report.job_directory.display());
        println!(
            "Cold image/cache preparation: {:.3}s",
            report.run_timing.cold_image_and_cache_ns as f64 / 1_000_000_000.0
        );
        match report.summary.known_estimated_cost_usd {
            Some(cost) => println!(
                "Known estimated cost: ${cost:.6} ({} of {} attempt{} priced)",
                report.summary.priced_attempts,
                report.summary.total,
                if report.summary.total == 1 { "" } else { "s" }
            ),
            None => {
                println!("Estimated cost: unavailable (provider usage was unavailable or unpriced)")
            }
        }
        if report.summary.observed_priced_attempts > report.summary.priced_attempts
            && let Some(cost) = report.summary.observed_estimated_cost_lower_bound_usd
        {
            println!(
                "Observed cost lower bound: ${cost:.6} ({} of {} attempt{} reported usage)",
                report.summary.observed_priced_attempts,
                report.summary.total,
                if report.summary.total == 1 { "" } else { "s" }
            );
        }
        if report.skipped > 0 {
            println!(
                "Resumed: {} previously completed attempt{} retained",
                report.skipped,
                if report.skipped == 1 { "" } else { "s" }
            );
        }
        if report.summary.failed + report.summary.refused + report.summary.errored > 0 {
            println!(
                "Inspect failures: nanocodex eval inspect {}",
                report.job_directory.display()
            );
        }
    }
}

fn persist_aggregate(job: &HarborJob, cold_image_and_cache: Duration) -> Result<()> {
    let invocation = load_required_invocation(job.directory())?;
    let aggregate = job
        .aggregate_dataset()?
        .with_run_identity(aggregate_run_identity(&invocation));
    write_json_atomic(
        &job.directory().join("aggregate.json"),
        &aggregate.with_run_timing(AggregateRunTiming {
            cold_image_and_cache_ns: duration_ns(cold_image_and_cache),
        }),
    )
}

fn aggregate_run_identity(invocation: &RunInvocation) -> AggregateRunIdentity {
    let vm = Some(AttemptVmIdentity {
        rootfs: invocation.vm_rootfs.clone(),
        guest_runtime_target: invocation
            .guest_runtime
            .as_ref()
            .map(|runtime| runtime.target.clone()),
        guest_runtime_sha256: invocation
            .guest_runtime
            .as_ref()
            .map(|runtime| runtime.binary_sha256.clone()),
        runtime_disk_digest: invocation
            .guest_runtime
            .as_ref()
            .and_then(|runtime| runtime.runtime_disk_digest.clone()),
    });
    AggregateRunIdentity {
        build: AttemptBuildIdentity {
            version: invocation.nanocodex_build.version.clone(),
            git_sha: Some(invocation.nanocodex_build.git_sha.clone()),
            built_at: Some(invocation.nanocodex_build.built_at.clone()),
            executable_sha256: Some(invocation.nanocodex_build.executable_sha256.clone()),
        },
        dataset_revision: None,
        model: invocation.model.clone(),
        model_tier: None,
        reasoning_effort: invocation.thinking.clone(),
        service_tier: Some("standard".to_owned()),
        tool_profile: invocation.tool_profile.clone(),
        seed: invocation.seed,
        agent_topology: "single_agent".to_owned(),
        vm,
    }
}

impl ResolvedRun {
    fn report_configuration(&self) {
        eprintln!(
            "Run config: thinking={} · trials={} · concurrency={} · environment=microVM · web_search={}",
            self.thinking, self.trials, self.concurrency, self.web_search
        );
        if let Some(runtime) = &self.vm_guest_runtime {
            eprintln!("VM guest runtime: pinned prebuilt {}", runtime.display());
        }
        eprintln!("VM cache: {}", self.vm_cache.display());
    }

    fn report_automatic_scheduling(&self) {
        let Some(automatic) = self.automatic_scheduling else {
            return;
        };
        let memory = automatic.host.physical_memory_bytes.map_or_else(
            || "unknown RAM".to_owned(),
            |bytes| format!("{} MiB RAM", bytes / BYTES_PER_MIB),
        );
        let concurrency_source = if automatic.concurrency {
            "automatic"
        } else {
            "configured"
        };
        let memory_source = if automatic.memory {
            "automatic"
        } else {
            "configured"
        };
        let max_memory = self
            .max_memory_mb
            .map_or_else(|| "unbounded".to_owned(), |memory| format!("{memory} MiB"));
        eprintln!(
            "Host scheduling: target={}%, detected={} logical CPUs/{memory}, \
             concurrency={} ({concurrency_source}), memory={max_memory} ({memory_source})",
            automatic.utilization_percent, automatic.host.logical_cpus, self.concurrency,
        );
    }

    fn invocation(&self, guest_runtime: Option<RetainedGuestRuntime>) -> Result<RunInvocation> {
        let scheduling = self.automatic_scheduling;
        let executable = std::env::current_exe()?;
        let executable_sha256 = stable_file_sha256(&executable)?;
        Ok(RunInvocation {
            version: INVOCATION_VERSION,
            nanocodex_build: RetainedBuild {
                version: env!("NANOCODEX_SEMVER_VERSION").to_owned(),
                git_sha: env!("VERGEN_GIT_SHA").to_owned(),
                built_at: env!("VERGEN_BUILD_TIMESTAMP").to_owned(),
                executable_sha256,
            },
            model: nanocodex::oai::MODEL.to_owned(),
            tool_profile: "microvm_workspace".to_owned(),
            seed: None,
            scheduling: RetainedScheduling {
                policy: SCHEDULING_POLICY.to_owned(),
                automatic_utilization_percent: scheduling
                    .map(|automatic| automatic.utilization_percent),
                concurrency_source: if scheduling.is_some_and(|automatic| automatic.concurrency) {
                    "automatic"
                } else {
                    "configured"
                }
                .to_owned(),
                memory_source: if scheduling.is_some_and(|automatic| automatic.memory) {
                    "automatic"
                } else {
                    "configured"
                }
                .to_owned(),
            },
            trials: self.trials,
            concurrency: self.concurrency,
            max_memory_mb: self.max_memory_mb,
            vm_rootfs: self.vm_rootfs.clone(),
            guest_runtime,
            vm_retention: self.vm_retention,
            thinking: self.thinking.to_string(),
            web_search: self.web_search,
            rerun_from: self.rerun_from.clone(),
        })
    }
}

fn report_resume(eval: &Evaluator, skipped: usize, total: usize) {
    if eval.resumed() {
        eprintln!(
            "Resuming Harbor job {} ({skipped} of {total} attempts already durable)",
            eval.directory().display(),
        );
    }
}

fn finish_run(error: Option<nanocodex_eval::EvalError>) -> Result<()> {
    error.map_or(Ok(()), |error| Err(error.into()))
}

fn load_prioritized_tasks(
    task_paths: Vec<PathBuf>,
    output: &Path,
) -> Result<(Vec<Task>, Duration)> {
    let started_at = Instant::now();
    let mut tasks = load_tasks(task_paths, Vec::new())?;
    prioritize_tasks(&mut tasks, output)?;
    Ok((tasks, started_at.elapsed()))
}

async fn prepare_run_vm(
    rootfs: Option<&Path>,
    guest_runtime: Option<&Path>,
    job: &Path,
    resumed: bool,
    rerun_from: Option<&Path>,
    allow_uninitialized_resume: bool,
) -> Result<(PathBuf, PathBuf, Option<RetainedGuestRuntime>, Duration)> {
    let vmm = std::env::current_exe()?;
    let started_at = Instant::now();
    let origin =
        retained_guest_runtime_origin(job, resumed, rerun_from, allow_uninitialized_resume)?;
    let runtime = prepare_runtime_for_vm(rootfs, guest_runtime, job, origin.as_ref()).await?;
    Ok((vmm, runtime.disk, runtime.identity, started_at.elapsed()))
}

struct RetainedGuestRuntimeOrigin {
    job: PathBuf,
    runtime: RetainedGuestRuntime,
}

fn retained_guest_runtime_origin(
    job: &Path,
    resumed: bool,
    rerun_from: Option<&Path>,
    allow_uninitialized_resume: bool,
) -> Result<Option<RetainedGuestRuntimeOrigin>> {
    let origin = if resumed { Some(job) } else { rerun_from };
    let Some(origin) = origin else {
        return Ok(None);
    };
    let Some(invocation) = load_invocation(origin)? else {
        if resumed && allow_uninitialized_resume {
            return Ok(None);
        }
        return Err(eyre!(
            "VM evaluation provenance is missing from {}; start a new job with --new",
            origin.join(INVOCATION_FILE).display()
        ));
    };
    let runtime = invocation.guest_runtime.ok_or_else(|| {
        eyre!(
            "VM guest runtime provenance is missing from {}; start a new job with --new",
            origin.join(INVOCATION_FILE).display()
        )
    })?;
    Ok(Some(RetainedGuestRuntimeOrigin {
        job: origin.to_path_buf(),
        runtime,
    }))
}

struct FinishedEvaluation {
    job: HarborJob,
    outcomes: Vec<AttemptOutcome>,
    results: Vec<EvalResult>,
    run_error: Option<nanocodex_eval::EvalError>,
    failed: usize,
    harbor_finish: Duration,
}

pub(super) struct DrainExecution<T, E> {
    pub(super) result: Result<T, E>,
    pub(super) terminal_attempts: usize,
    pub(super) interrupted: bool,
    pub(super) interrupt: InterruptListener,
}

pub(super) enum InterruptListener {
    #[cfg(unix)]
    Unix(tokio::signal::unix::Signal),
    #[cfg(windows)]
    Windows(tokio::signal::windows::CtrlC),
    #[cfg(not(any(unix, windows)))]
    Fallback,
    #[cfg(test)]
    Injected(tokio::sync::mpsc::UnboundedReceiver<io::Result<()>>),
}

impl InterruptListener {
    async fn recv(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(signal) => signal
                .recv()
                .await
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "Ctrl-C listener closed")),
            #[cfg(windows)]
            Self::Windows(signal) => signal
                .recv()
                .await
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "Ctrl-C listener closed")),
            #[cfg(not(any(unix, windows)))]
            Self::Fallback => tokio::signal::ctrl_c().await,
            #[cfg(test)]
            Self::Injected(signals) => signals.recv().await.unwrap_or_else(|| {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected Ctrl-C listener closed",
                ))
            }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum EvalInterruptError {
    #[error("failed to listen for Ctrl-C: {0}")]
    Listener(#[source] io::Error),
    #[error("second interrupt received; aborted admitted evaluation work")]
    Forced,
    #[error("interrupt received during evaluation finalization")]
    Finalization,
}

pub(super) fn ctrl_c_interrupt() -> io::Result<InterruptListener> {
    #[cfg(unix)]
    {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map(InterruptListener::Unix)
    }
    #[cfg(windows)]
    {
        tokio::signal::windows::ctrl_c().map(InterruptListener::Windows)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(InterruptListener::Fallback)
    }
}

pub(super) async fn finish_or_drain<T, E, Work, Drain>(
    work: Work,
    mut interrupt: InterruptListener,
    terminal_attempts: usize,
    drain: Drain,
) -> Result<DrainExecution<T, E>, EvalInterruptError>
where
    Work: Future<Output = Result<T, E>>,
    Drain: FnOnce() -> usize,
{
    tokio::pin!(work);
    tokio::select! {
        biased;
        signal = interrupt.recv() => {
            signal.map_err(EvalInterruptError::Listener)?;
            let terminal_attempts = drain();
            tokio::select! {
                biased;
                signal = interrupt.recv() => {
                    signal.map_err(EvalInterruptError::Listener)?;
                    Err(EvalInterruptError::Forced)
                }
                result = &mut work => Ok(DrainExecution {
                    result,
                    terminal_attempts,
                    interrupted: true,
                    interrupt,
                })
            }
        }
        result = &mut work => Ok(DrainExecution {
            result,
            terminal_attempts,
            interrupted: false,
            interrupt,
        }),
    }
}

pub(super) async fn finish_or_interrupt<T, Work>(
    work: Work,
    mut interrupt: InterruptListener,
) -> Result<T, EvalInterruptError>
where
    Work: Future<Output = T>,
{
    tokio::pin!(work);
    tokio::select! {
        biased;
        signal = interrupt.recv() => {
            signal.map_err(EvalInterruptError::Listener)?;
            Err(EvalInterruptError::Finalization)
        }
        output = &mut work => Ok(output),
    }
}

async fn finish_evaluation(
    harbor: HarborRecorder,
    remaining_attempts: usize,
    progress: tokio::task::JoinHandle<Result<Progress>>,
    sweep_result: Result<SweepResults, nanocodex_eval::EvalError>,
) -> Result<FinishedEvaluation> {
    let started_at = Instant::now();
    let job = harbor.finish_all(remaining_attempts).await?;
    let progress = progress.await??;
    let (outcomes, results, run_error, failed) = match sweep_result {
        Ok(results) => {
            let outcomes = results
                .into_outcomes()
                .into_iter()
                .map(AttemptOutcome::from_terminal)
                .collect::<Vec<_>>();
            let failed = outcomes
                .iter()
                .filter(|outcome| outcome.has_lifecycle_error())
                .count();
            let results = scored_results(&outcomes);
            (outcomes, results, None, failed)
        }
        Err(error) => {
            let results = progress.scored_results();
            (progress.outcomes, results, Some(error), progress.failed)
        }
    };
    Ok(FinishedEvaluation {
        job,
        outcomes,
        results,
        run_error,
        failed,
        harbor_finish: started_at.elapsed(),
    })
}

fn bind_finite_run(evaluator: EvaluatorBuilder, sweep: Sweep, fresh: bool) -> EvaluatorBuilder {
    if fresh {
        evaluator.fresh_run(sweep)
    } else {
        evaluator.resume_incomplete(sweep)
    }
}

const fn configure_memory_limit(
    evaluator: EvaluatorBuilder,
    max_memory_mb: Option<u64>,
) -> EvaluatorBuilder {
    match max_memory_mb {
        Some(max_memory_mb) => evaluator.max_memory_mb(max_memory_mb),
        None => evaluator,
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum VmRetention {
    /// Retain disks only for failures, refusals, and errors.
    #[default]
    Failures,
    /// Retain disks for every attempt, including passes.
    All,
}

impl VmRetention {
    const fn retains_passes(self) -> bool {
        matches!(self, Self::All)
    }
}

pub(super) fn load_tasks(paths: Vec<PathBuf>, suites: Vec<PathBuf>) -> Result<Vec<Task>> {
    load_task_paths(paths, suites)?
        .into_iter()
        .map(Task::load)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn prioritize_tasks(tasks: &mut [Task], output: &Path) -> Result<()> {
    let estimates = retained_task_durations(output)?;
    tasks.sort_by_key(|task| {
        let declared_floor = task
            .agent_timeout()
            .div_f64(4.0)
            .min(Duration::from_mins(10));
        let estimate = estimates
            .get(task.name())
            .copied()
            .unwrap_or(declared_floor);
        Reverse((estimate, task.agent_timeout(), task.verifier().timeout()))
    });
    Ok(())
}

fn retained_task_durations(output: &Path) -> Result<BTreeMap<String, Duration>> {
    let jobs = match fs::read_dir(output) {
        Ok(jobs) => jobs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let mut samples = BTreeMap::<String, Vec<Duration>>::new();
    for job in jobs {
        let job = job?;
        if !job.file_type()?.is_dir() {
            continue;
        }
        for trial in fs::read_dir(job.path())? {
            let trial = trial?;
            if !trial.file_type()?.is_dir() {
                continue;
            }
            let Ok(bytes) = fs::read(trial.path().join("result.json")) else {
                continue;
            };
            let Ok(result) = serde_json::from_slice::<RetainedTrialTiming>(&bytes) else {
                continue;
            };
            if result.exception_info.as_ref().is_some_and(|exception| {
                matches!(
                    exception.exception_type.as_str(),
                    "EnvironmentError" | "VerifierError" | "NanocodexEvalError"
                )
            }) {
                continue;
            }
            let Ok(duration) = result
                .finished_at
                .signed_duration_since(result.started_at)
                .to_std()
            else {
                continue;
            };
            samples.entry(result.task_name).or_default().push(duration);
        }
    }
    Ok(samples
        .into_iter()
        .map(|(task, mut durations)| {
            durations.sort_unstable();
            let median = durations[durations.len() / 2];
            (task, median)
        })
        .collect())
}

#[derive(Deserialize)]
struct RetainedTrialTiming {
    task_name: String,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    exception_info: Option<RetainedException>,
}

#[derive(Deserialize)]
struct RetainedException {
    exception_type: String,
}

pub(crate) fn load_task_paths(
    mut paths: Vec<PathBuf>,
    suites: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    for suite in suites {
        let mut suite_tasks = fs::read_dir(&suite)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path.join("task.toml").is_file())
            .collect::<Vec<_>>();
        suite_tasks.sort();
        if suite_tasks.is_empty() {
            return Err(eyre!(
                "suite contains no immediate task directories: {}",
                suite.display()
            ));
        }
        paths.extend(suite_tasks);
    }
    Ok(paths)
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "run/tests.rs"]
mod tests;
