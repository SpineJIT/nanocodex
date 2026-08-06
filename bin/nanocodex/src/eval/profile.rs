use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Args, ValueEnum};
use eyre::{Result, WrapErr as _, eyre};
use nanocodex::Model;
use nanocodex_eval::{
    CoordinateClaim, EvalOutcome, Evaluation, EvaluationClaim, EvaluationMode, EvaluationSelector,
    Evaluator, Task,
    differential::{
        CodexAuth, CodexToolMode, DifferentialEvaluator, ExecutableIdentity, NanocodexToolMode,
    },
    vm::{CachePolicy, VmResources},
};
use serde::Serialize;

use super::{args::VmPreparationArgs, run};
use crate::{
    config::{EvalAgentArgs, SharedAuth},
    observability::ObservabilityArgs,
};

const CONFIG_FILE: &str = "nanocodex.toml";
const LEASE_DURATION: Duration = Duration::from_secs(5 * 60);
const DEFAULT_INITIAL_GUEST_MEMORY_MB: u64 = 512;
const MEMORY_PROFILE_FILE: &str = "differential-memory-profiles.json";

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

    /// Select one Nanocodex tool treatment from the profile.
    #[arg(long, value_enum)]
    nanocodex_tool_mode: Option<ToolMode>,

    /// Select one stock-Codex tool treatment from the profile.
    #[arg(long, value_enum)]
    codex_tool_mode: Option<ToolMode>,

    #[command(flatten)]
    vm: VmPreparationArgs,

    /// Initial eval-only guest RAM allocated to each differential arm.
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

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ToolMode {
    #[value(alias = "code_mode")]
    CodeMode,
    #[value(alias = "code_mode_only")]
    CodeModeOnly,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum RunOutput<'a> {
    Completed {
        profile: &'a str,
        task: &'a str,
        repetition: u16,
        evidence: &'a Path,
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
    pub(super) fn run(self) -> Result<()> {
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
        let evaluation = self.target.open()?;
        let requested_thinking = self.agent.thinking();
        if self
            .agent
            .web_search()
            .is_some_and(|requested| requested != evaluation.web_search())
        {
            return Err(eyre!(
                "--web-search cannot override profile `{}`; the profile fixes web_search={}",
                evaluation.name(),
                evaluation.web_search()
            ));
        }
        let selector = EvaluationSelector::new(&self.task)
            .model(self.model)
            .thinking(requested_thinking)
            .nanocodex_tool_mode(self.nanocodex_tool_mode.map(NanocodexToolMode::from))
            .codex_tool_mode(self.codex_tool_mode.map(CodexToolMode::from));
        let mut prepared = None;
        loop {
            match evaluation.claim(&selector, LEASE_DURATION)? {
                EvaluationClaim::Prepare(claim) => {
                    let result = prepare_resources(claim.task(), &self.vm).await;
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
                            None => prepare_resources(claim.task(), &self.vm).await?,
                        };
                        execute_coordinate(
                            &claim,
                            resources,
                            self.guest_memory_mb,
                            self.agent,
                            &self.vm,
                        )
                        .await
                    }
                    .await;
                    match result {
                        Ok(ExecutionResult::Accepted(evidence)) => {
                            let repetition = claim.repetition();
                            claim.complete(&evidence)?;
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

async fn prepare_resources(task: &Task, vm: &VmPreparationArgs) -> Result<VmResources> {
    let current_executable = std::env::current_exe()?;
    let runtime_image =
        run::prepare_vm_guest_runtime_from(vm.vm_guest_runtime.as_deref(), &vm.vm_cache).await?;
    Ok(VmResources::builder(&current_executable, runtime_image)
        .task(task.clone())
        .cache_directory(&vm.vm_cache)
        .cache_policy(if vm.vm_refresh {
            CachePolicy::Refresh
        } else {
            CachePolicy::Reuse
        })
        .image_preparation_concurrency(1)
        .prepare()
        .await?)
}

#[allow(clippy::too_many_arguments)]
async fn execute_coordinate(
    claim: &CoordinateClaim,
    resources: VmResources,
    guest_memory_mb: u64,
    agent: EvalAgentArgs,
    vm: &VmPreparationArgs,
) -> Result<ExecutionResult> {
    let treatment = claim.treatment();
    let task = claim.task().clone();
    let output = claim.output_directory();
    std::fs::create_dir_all(output)?;
    match treatment.mode {
        EvaluationMode::Nanocodex => {
            let backend = resources.backend().await?;
            let nanocodex =
                agent.builder(treatment.model, treatment.thinking, claim.web_search())?;
            let evaluator = Evaluator::builder(nanocodex, backend)
                .output_directory(output)
                .build()?;
            let outcome = evaluator.task(task).await?;
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
        EvaluationMode::Differential => {
            let (nanocodex, auth) =
                agent.shared_builder(treatment.model, treatment.thinking, claim.web_search())?;
            let codex_auth = match auth {
                SharedAuth::ApiKey(api_key) => CodexAuth::api_key(api_key),
                SharedAuth::AuthFile(path) => CodexAuth::auth_file(path),
            };
            let executable = std::env::current_exe()?;
            let codex = claim
                .codex_command()
                .ok_or_else(|| eyre!("differential profile lost its Codex command"))?;
            let evaluator = DifferentialEvaluator::builder(nanocodex)
                .codex(codex, codex_auth)
                .vm(resources)
                .output_directory(output)
                .thinking(treatment.thinking)
                .web_search(claim.web_search())
                .nanocodex_tool_mode(treatment.nanocodex_tool_mode)
                .codex_tool_mode(treatment.codex_tool_mode)
                .nanocodex_executable(
                    ExecutableIdentity::new(executable, env!("NANOCODEX_SEMVER_VERSION"))
                        .git_sha(env!("VERGEN_GIT_SHA"))
                        .built_at(env!("VERGEN_BUILD_TIMESTAMP")),
                )
                .initial_guest_memory_mb(guest_memory_mb)
                .memory_profile_path(vm.vm_cache.join(MEMORY_PROFILE_FILE))
                .prepare()
                .await?;
            let report = evaluator.task(task).await?;
            let evidence = report.comparison_path().to_path_buf();
            if report.has_infrastructure_failure() || report.has_operational_error() {
                Ok(ExecutionResult::Retryable {
                    error: "differential pair retained infrastructure or operational failure"
                        .to_owned(),
                    evidence,
                })
            } else {
                Ok(ExecutionResult::Accepted(evidence))
            }
        }
    }
}

fn default_state_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("NANOCODEX_HOME") {
        return Ok(PathBuf::from(home).join("evals"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| eyre!("HOME is not set; pass --state-dir for durable eval state"))?;
    Ok(PathBuf::from(home).join(".nanocodex/evals"))
}

fn write_json(value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(std::io::stdout().lock(), value)?;
    println!();
    Ok(())
}

impl From<ToolMode> for NanocodexToolMode {
    fn from(value: ToolMode) -> Self {
        match value {
            ToolMode::CodeMode => Self::CodeMode,
            ToolMode::CodeModeOnly => Self::CodeModeOnly,
        }
    }
}

impl From<ToolMode> for CodexToolMode {
    fn from(value: ToolMode) -> Self {
        match value {
            ToolMode::CodeMode => Self::CodeMode,
            ToolMode::CodeModeOnly => Self::CodeModeOnly,
        }
    }
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
