use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Args;
use eyre::{Result, WrapErr as _, eyre};
use nanocodex::Model;
use nanocodex_eval::{
    EvalAttemptOutcome, EvalEventKind, EvalEventStream, EvalOutcome, Evaluation, EvaluationClaim,
    EvaluationSelection, EvaluationSelector, Evaluator, ResolvedHarness, Task,
    atif::AtifBuilder,
    coordinator::{CoordinatorClient, RemoteClaim, RemoteLease},
    harness::{Harness, HarnessAuth},
    vm::{CachePolicy, VmResources},
};
use serde::Serialize;
use tokio::io::AsyncWriteExt as _;

use super::{args::VmPreparationArgs, run};
use crate::{
    config::{EvalAgentArgs, SharedAuth},
    observability::ObservabilityArgs,
};

const CONFIG_FILE: &str = "nanocodex.toml";
const LEASE_DURATION: Duration = Duration::from_secs(5 * 60);
const DEFAULT_INITIAL_GUEST_MEMORY_MB: u64 = 512;

#[derive(Clone, Debug, Args)]
pub(super) struct ProfileTarget {
    /// Evaluation profile. Uses the manifest's top-level default when omitted.
    profile: Option<String>,

    /// Evaluation manifest containing the closed desired work bundle.
    #[arg(long, default_value = CONFIG_FILE)]
    config: PathBuf,

    /// Durable SQLite ledger and retained artifacts.
    ///
    /// Defaults to ~/.nanocodex/evals.
    #[arg(long, value_name = "DIRECTORY")]
    state_dir: Option<PathBuf>,

    /// Pull claims from a remote coordinator instead of opening SQLite directly.
    #[arg(long, value_name = "URL", conflicts_with = "state_dir")]
    coordinator: Option<String>,
}

#[derive(Args)]
pub(super) struct Status {
    #[command(flatten)]
    target: ProfileTarget,

    /// Print the complete machine-readable profile ledger.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(super) struct Run {
    #[command(flatten)]
    target: ProfileTarget,

    /// Exact task selector from the configured profile.
    #[arg(long, value_name = "TASK", required = true)]
    task: String,

    /// Select one model when the profile contains a model matrix.
    #[arg(long)]
    model: Option<Model>,

    /// Select one configured external harness. Omission uses Nanocodex.
    #[arg(long, value_name = "NAME")]
    harness: Option<String>,

    /// Advisory stable name used for coordinator task affinity and status.
    #[arg(long, env = "NANOCODEX_WORKER_NAME", value_name = "NAME")]
    worker: Option<String>,

    #[command(flatten)]
    vm: VmPreparationArgs,

    /// Guest RAM allocated to an external harness sandbox.
    #[arg(
        long,
        value_name = "MIB",
        default_value_t = DEFAULT_INITIAL_GUEST_MEMORY_MB,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    guest_memory_mb: u64,

    #[command(flatten)]
    observability: ObservabilityArgs,

    #[command(flatten)]
    agent: EvalAgentArgs,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum RunOutput<'a> {
    Completed {
        profile: &'a str,
        task: &'a str,
        repetition: u16,
        evidence: &'a str,
    },
    AlreadyComplete {
        profile: &'a str,
        task: &'a str,
    },
    TemporarilyUnavailable {
        profile: &'a str,
        task: &'a str,
        reason: &'a str,
        retry_after_ms: u64,
    },
}

impl Status {
    pub(super) async fn run(self) -> Result<()> {
        if let Some(coordinator) = &self.target.coordinator {
            let status = CoordinatorClient::new(coordinator)?.status().await?;
            if self.json {
                serde_json::to_writer_pretty(std::io::stdout().lock(), &status)?;
                println!();
            } else {
                print_remote_status(&status);
            }
            return Ok(());
        }
        let evaluation = self.target.open()?;
        let status = evaluation.status()?;
        if self.json {
            serde_json::to_writer_pretty(std::io::stdout().lock(), &status)?;
            println!();
        } else {
            println!(
                "{} {} · preparation {}/{} ready · coordinates {}/{} terminal, {} running",
                status.profile,
                &status.digest[..12],
                status.preparation.complete,
                status.preparation.pending
                    + status.preparation.running
                    + status.preparation.complete,
                status.coordinates.complete,
                status.coordinates.pending
                    + status.coordinates.running
                    + status.coordinates.complete,
                status.coordinates.running,
            );
            for family in status.families {
                println!(
                    "  {} · {}/{} terminal · {} running · {} pending",
                    family.task, family.complete, family.desired, family.running, family.pending
                );
            }
        }
        Ok(())
    }
}

impl Run {
    pub(super) async fn run(self) -> Result<()> {
        let _observability = self.observability.install(false, Path::new("."))?;
        let requested_thinking = self.agent.thinking();
        let selector = EvaluationSelector::new(&self.task)
            .harness(self.harness)
            .model(self.model)
            .thinking(requested_thinking);
        if let Some(coordinator) = &self.target.coordinator {
            let selection = EvaluationSelection::load(
                &self.target.config,
                self.target.profile.as_deref(),
                &selector,
            )?;
            validate_web_search(&self.agent, selection.profile(), selection.web_search())?;
            let mut coordinator = CoordinatorClient::new(coordinator)?;
            if let Some(worker) = self.worker {
                coordinator = coordinator.worker(worker);
            }
            return run_remote(
                coordinator,
                selection,
                &self.task,
                self.guest_memory_mb,
                self.agent,
                &self.vm,
            )
            .await;
        }
        let evaluation = self.target.open()?;
        validate_web_search(&self.agent, evaluation.name(), evaluation.web_search())?;
        let mut prepared = None;
        loop {
            match evaluation.claim(&selector, LEASE_DURATION)? {
                EvaluationClaim::Prepare(claim) => {
                    let result = prepare_resources(claim.task(), claim.harnesses(), &self.vm).await;
                    match result {
                        Ok(resources) => {
                            claim.complete()?;
                            prepared = Some(resources);
                        }
                        Err(error) => {
                            claim.retry(&format!("{error:#}"))?;
                            return Err(error).wrap_err("task preparation remains retryable");
                        }
                    }
                }
                EvaluationClaim::Run(claim) => {
                    let result = async {
                        let resources = match prepared.take() {
                            Some(resources) => resources,
                            None => {
                                prepare_resources(claim.task(), claim.harnesses(), &self.vm).await?
                            }
                        };
                        execute_coordinate(
                            claim.task().clone(),
                            claim.treatment().clone(),
                            claim.web_search(),
                            claim.harness().cloned(),
                            claim.output_directory().to_path_buf(),
                            resources,
                            self.guest_memory_mb,
                            self.agent,
                        )
                        .await
                    }
                    .await;
                    match result {
                        Ok(ExecutionResult::Accepted(evidence)) => {
                            let repetition = claim.repetition();
                            claim.complete(&evidence)?;
                            let evidence = evidence.to_string_lossy();
                            write_json(&RunOutput::Completed {
                                profile: evaluation.name(),
                                task: &self.task,
                                repetition,
                                evidence: &evidence,
                            })?;
                            return Ok(());
                        }
                        Ok(ExecutionResult::Retryable { error, evidence }) => {
                            claim.retry(&error)?;
                            return Err(eyre!(
                                "coordinate remains retryable; evidence retained at {}: {error}",
                                evidence.display()
                            ));
                        }
                        Err(error) => {
                            claim.retry(&format!("{error:#}"))?;
                            return Err(error).wrap_err("coordinate remains retryable");
                        }
                    }
                }
                EvaluationClaim::Busy(busy) => {
                    write_json(&RunOutput::TemporarilyUnavailable {
                        profile: evaluation.name(),
                        task: &self.task,
                        reason: busy.reason,
                        retry_after_ms: busy.retry_after_ms,
                    })?;
                    return Err(eyre!(
                        "temporarily unavailable: {}; retry after {} ms",
                        busy.reason,
                        busy.retry_after_ms
                    ));
                }
                EvaluationClaim::Complete => {
                    write_json(&RunOutput::AlreadyComplete {
                        profile: evaluation.name(),
                        task: &self.task,
                    })?;
                    return Ok(());
                }
            }
        }
    }
}

async fn run_remote(
    coordinator: CoordinatorClient,
    selection: EvaluationSelection,
    task_selector: &str,
    guest_memory_mb: u64,
    agent: EvalAgentArgs,
    vm: &VmPreparationArgs,
) -> Result<()> {
    let mut prepared = None;
    loop {
        match coordinator.claim(&selection).await? {
            RemoteClaim::Prepare(lease) => {
                let heartbeat = remote_heartbeat(coordinator.clone(), lease.clone());
                let result = prepare_resources(selection.task(), selection.harnesses(), vm).await;
                match result {
                    Ok(resources) => {
                        let finish = coordinator.prepared(&lease).await;
                        heartbeat.abort();
                        finish?;
                        prepared = Some(resources);
                    }
                    Err(error) => {
                        let finish = coordinator.retry(&lease, &format!("{error:#}")).await;
                        heartbeat.abort();
                        finish?;
                        return Err(error).wrap_err("remote task preparation remains retryable");
                    }
                }
            }
            RemoteClaim::Run { lease, repetition } => {
                std::fs::create_dir_all(&vm.vm_cache)?;
                let output = tempfile::Builder::new()
                    .prefix("nanocodex-eval-worker-")
                    .tempdir_in(&vm.vm_cache)?;
                let heartbeat = remote_heartbeat(coordinator.clone(), lease.clone());
                let result = async {
                    let resources = match prepared.take() {
                        Some(resources) => resources,
                        None => {
                            prepare_resources(selection.task(), selection.harnesses(), vm).await?
                        }
                    };
                    execute_coordinate(
                        selection.task().clone(),
                        selection.treatment().clone(),
                        selection.web_search(),
                        selection.harness().cloned(),
                        output.path().to_path_buf(),
                        resources,
                        guest_memory_mb,
                        agent,
                    )
                    .await
                }
                .await;
                match result {
                    Ok(ExecutionResult::Accepted(evidence)) => {
                        let finish = coordinator.complete(&lease, output.path(), &evidence).await;
                        heartbeat.abort();
                        finish?;
                        write_json(&RunOutput::Completed {
                            profile: selection.profile(),
                            task: task_selector,
                            repetition,
                            evidence: "coordinator",
                        })?;
                        return Ok(());
                    }
                    Ok(ExecutionResult::Retryable { error, .. }) => {
                        let finish = async {
                            coordinator.upload(&lease, output.path()).await?;
                            coordinator.retry(&lease, &error).await
                        }
                        .await;
                        heartbeat.abort();
                        finish?;
                        return Err(eyre!("remote coordinate remains retryable: {error}"));
                    }
                    Err(error) => {
                        let finish = async {
                            coordinator.upload(&lease, output.path()).await?;
                            coordinator.retry(&lease, &format!("{error:#}")).await
                        }
                        .await;
                        heartbeat.abort();
                        finish?;
                        return Err(error).wrap_err("remote coordinate remains retryable");
                    }
                }
            }
            RemoteClaim::Busy {
                reason,
                retry_after_ms,
            } => {
                write_json(&RunOutput::TemporarilyUnavailable {
                    profile: selection.profile(),
                    task: task_selector,
                    reason: &reason,
                    retry_after_ms,
                })?;
                return Err(eyre!(
                    "temporarily unavailable: {reason}; retry after {retry_after_ms} ms"
                ));
            }
            RemoteClaim::Complete => {
                write_json(&RunOutput::AlreadyComplete {
                    profile: selection.profile(),
                    task: task_selector,
                })?;
                return Ok(());
            }
        }
    }
}

fn remote_heartbeat(
    coordinator: CoordinatorClient,
    lease: RemoteLease,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(20));
        interval.tick().await;
        loop {
            interval.tick().await;
            if coordinator.heartbeat(&lease).await.is_err() {
                return;
            }
        }
    })
}

fn validate_web_search(agent: &EvalAgentArgs, profile: &str, web_search: bool) -> Result<()> {
    if agent
        .web_search()
        .is_some_and(|requested| requested != web_search)
    {
        return Err(eyre!(
            "--web-search cannot override profile `{profile}`; the profile fixes web_search={web_search}"
        ));
    }
    Ok(())
}

impl ProfileTarget {
    fn open(&self) -> Result<Evaluation> {
        let state_directory = self.state_dir.clone().map_or_else(default_state_dir, Ok)?;
        Ok(Evaluation::open(
            &self.config,
            self.profile.as_deref(),
            state_directory,
        )?)
    }
}

enum ExecutionResult {
    Accepted(PathBuf),
    Retryable { error: String, evidence: PathBuf },
}

async fn prepare_resources(
    task: &Task,
    harnesses: &[ResolvedHarness],
    vm: &VmPreparationArgs,
) -> Result<VmResources> {
    let current_executable = std::env::current_exe()?;
    let runtime_image =
        run::prepare_vm_guest_runtime_from(vm.vm_guest_runtime.as_deref(), &vm.vm_cache).await?;
    let mut builder = VmResources::builder(&current_executable, runtime_image)
        .task(task.clone())
        .cache_directory(&vm.vm_cache)
        .cache_policy(if vm.vm_refresh {
            CachePolicy::Refresh
        } else {
            CachePolicy::Reuse
        })
        .image_preparation_concurrency(1);
    for harness in harnesses {
        builder = builder.guest_executable(&harness.command, &harness.guest_command);
    }
    let resources = builder.prepare().await?;
    // The durable preparation lease covers the complete immutable task
    // environment, not merely the lazy recipe used to build it.
    resources.backend().await?;
    Ok(resources)
}

#[allow(clippy::too_many_arguments)]
async fn execute_coordinate(
    task: Task,
    treatment: nanocodex_eval::EvaluationTreatment,
    web_search: bool,
    harness: Option<ResolvedHarness>,
    output: PathBuf,
    resources: VmResources,
    guest_memory_mb: u64,
    agent: EvalAgentArgs,
) -> Result<ExecutionResult> {
    std::fs::create_dir_all(&output)?;
    match treatment.harness.as_str() {
        "nanocodex" => {
            let backend = resources.backend().await?;
            let nanocodex = agent.builder(treatment.model, treatment.thinking, web_search)?;
            let evaluator = Evaluator::builder(nanocodex, backend)
                .output_directory(&output)
                .build()?;
            let outcome = run_native(&evaluator, task).await?;
            let evidence = evaluator.directory().to_path_buf();
            if outcome.outcome() == EvalOutcome::InfrastructureError {
                Ok(ExecutionResult::Retryable {
                    error: "native evaluator retained an infrastructure failure".to_owned(),
                    evidence,
                })
            } else {
                Ok(ExecutionResult::Accepted(evidence))
            }
        }
        _ => {
            let (nanocodex, auth) =
                agent.shared_builder(treatment.model, treatment.thinking, web_search)?;
            let harness_auth = match auth {
                SharedAuth::ApiKey(api_key) => HarnessAuth::api_key(api_key),
                SharedAuth::AuthFile(path) => HarnessAuth::auth_file(path),
            };
            let configured =
                harness.ok_or_else(|| eyre!("external harness lost its resolved configuration"))?;
            let harness = Harness::new(
                nanocodex,
                task.clone(),
                &configured.command,
                &configured.guest_command,
                harness_auth,
                resources,
            )
            .model(treatment.model)
            .output_directory(&output)
            .thinking(treatment.thinking)
            .web_search(web_search)
            .guest_memory_mb(guest_memory_mb)
            .arguments(configured.arguments)
            .environment(configured.environment.into_iter().collect())
            .credentials(
                configured.home,
                configured.auth_file,
                configured.api_key_environment,
            )
            .api_upstream(configured.api_upstream)
            .version(configured.version)
            .name(configured.name)
            .prepare()
            .await?;
            let outcome = run_native(harness.evaluator(), task).await?;
            harness.retain_trajectory(&outcome).await?;
            let evidence = harness.directory().to_path_buf();
            if outcome.outcome() == EvalOutcome::InfrastructureError {
                Ok(ExecutionResult::Retryable {
                    error: "external harness retained an infrastructure failure".to_owned(),
                    evidence,
                })
            } else {
                Ok(ExecutionResult::Accepted(evidence))
            }
        }
    }
}

struct NativeEventRecording {
    atif: AtifBuilder,
    atif_error: Option<String>,
}

async fn run_native(evaluator: &Evaluator, task: Task) -> Result<EvalAttemptOutcome> {
    let event_log = evaluator.directory().join("events.jsonl");
    let run = evaluator.task(task);
    let stream = run.events().subscribe();
    let recorder = tokio::spawn(async move { record_native_events(stream, &event_log).await });
    let outcome = run.await;
    let recording = recorder
        .await
        .wrap_err("native event recorder task failed")??;
    let outcome = outcome?;
    retain_native_trajectory(&outcome, recording).await?;
    Ok(outcome)
}

async fn record_native_events(
    mut stream: EvalEventStream,
    path: &Path,
) -> Result<NativeEventRecording> {
    let mut output = tokio::fs::File::create(path)
        .await
        .wrap_err_with(|| format!("failed to create evaluator event log {}", path.display()))?;
    let mut atif = AtifBuilder::default();
    let mut atif_error = None;
    while let Some(event) = stream.recv().await? {
        if let EvalEventKind::Agent(agent_event) = &event.kind
            && atif_error.is_none()
            && let Err(error) = atif.apply(agent_event)
        {
            atif_error = Some(format!(
                "failed to project agent event sequence {} into ATIF: {error}",
                event.sequence
            ));
        }
        let mut encoded = serde_json::to_vec(event.as_ref())?;
        encoded.push(b'\n');
        output.write_all(&encoded).await?;
    }
    output.flush().await?;
    output.sync_all().await?;
    Ok(NativeEventRecording { atif, atif_error })
}

async fn retain_native_trajectory(
    outcome: &EvalAttemptOutcome,
    recording: NativeEventRecording,
) -> Result<()> {
    if let Some(error) = recording.atif_error {
        return Err(eyre!(error));
    }
    let trajectory = match outcome.agent() {
        Some(agent) => recording.atif.finish(outcome.task(), agent),
        None => recording.atif.finish_failure(outcome.task()),
    };
    let path = outcome.artifacts().directory.join("agent/trajectory.json");
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("trajectory path has no parent: {}", path.display()))?;
    tokio::fs::create_dir_all(parent).await?;
    let mut encoded = serde_json::to_vec_pretty(&trajectory)?;
    encoded.push(b'\n');
    let mut output = tokio::fs::File::create(&path)
        .await
        .wrap_err_with(|| format!("failed to create trajectory {}", path.display()))?;
    output.write_all(&encoded).await?;
    output.flush().await?;
    output.sync_all().await?;
    Ok(())
}

pub(super) fn default_state_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("NANOCODEX_HOME") {
        return Ok(PathBuf::from(home).join("evals"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| eyre!("HOME is not set; pass --state-dir for durable eval state"))?;
    Ok(PathBuf::from(home).join(".nanocodex/evals"))
}

fn print_remote_status(status: &serde_json::Value) {
    let profile = status["profile"].as_str().unwrap_or("unknown");
    let digest = status["digest"].as_str().unwrap_or("unknown");
    let preparation = &status["preparation"];
    let coordinates = &status["coordinates"];
    println!(
        "{} {} · preparation {}/{} ready · coordinates {}/{} terminal, {} running",
        profile,
        &digest[..digest.len().min(12)],
        preparation["complete"].as_u64().unwrap_or(0),
        count_total(preparation),
        coordinates["complete"].as_u64().unwrap_or(0),
        count_total(coordinates),
        coordinates["running"].as_u64().unwrap_or(0),
    );
    for family in status["families"].as_array().into_iter().flatten() {
        println!(
            "  {} · {}/{} terminal · {} running · {} pending",
            family["task"].as_str().unwrap_or("unknown"),
            family["complete"].as_u64().unwrap_or(0),
            family["desired"].as_u64().unwrap_or(0),
            family["running"].as_u64().unwrap_or(0),
            family["pending"].as_u64().unwrap_or(0),
        );
    }
}

fn count_total(counts: &serde_json::Value) -> u64 {
    ["pending", "running", "complete"]
        .into_iter()
        .map(|key| counts[key].as_u64().unwrap_or(0))
        .sum()
}

fn write_json(value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(std::io::stdout().lock(), value)?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser as _;

    use super::default_state_dir;
    use crate::{Cli, Command, eval::EvalCommand};

    #[test]
    fn run_requires_an_explicit_profile_task_but_no_trial_number() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "run",
            "release",
            "--task",
            "terminal/fix-git",
            "--worker",
            "dev-one",
            "--api-key",
            "test-key",
        ])
        .unwrap();
        let Some(Command::Eval(eval)) = cli.command else {
            panic!("expected eval command");
        };
        let EvalCommand::Run(run) = eval.command else {
            panic!("expected profile run");
        };
        assert_eq!(run.task, "terminal/fix-git");
        assert_eq!(run.worker.as_deref(), Some("dev-one"));
    }

    #[test]
    fn run_accepts_one_optional_external_harness() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "run",
            "release",
            "--task",
            "terminal/fix-git",
            "--harness",
            "codex",
            "--api-key",
            "test-key",
        ])
        .unwrap();
        let Some(Command::Eval(eval)) = cli.command else {
            panic!("expected eval command");
        };
        let EvalCommand::Run(run) = eval.command else {
            panic!("expected profile run");
        };
        assert_eq!(run.harness.as_deref(), Some("codex"));
    }

    #[test]
    fn explicit_state_directory_is_optional() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "status",
            "release",
            "--state-dir",
            "/mnt/evals",
        ])
        .unwrap();
        let Some(Command::Eval(eval)) = cli.command else {
            panic!("expected eval command");
        };
        let EvalCommand::Status(status) = eval.command else {
            panic!("expected profile status");
        };
        assert_eq!(
            status.target.state_dir.as_deref(),
            Some(Path::new("/mnt/evals"))
        );
    }

    #[test]
    fn nanocodex_home_owns_the_default_eval_directory() {
        let path = default_state_dir().unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("evals")
        );
    }
}
