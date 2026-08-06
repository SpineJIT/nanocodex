use std::{
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use clap::{Args, ValueEnum};
use eyre::{Result, eyre};
use nanocodex::Model;
use nanocodex_eval::{
    Task,
    differential::{
        CodexAuth, CodexToolMode, DifferentialEvaluator, ExecutableIdentity, NanocodexToolMode,
        reanalyze,
    },
    vm::{CachePolicy, VmResources},
};

use super::{args::VmPreparationArgs, run};
use crate::{
    config::{EvalAgentArgs, SharedAuth},
    observability::ObservabilityArgs,
};

const DEFAULT_OUTPUT_DIRECTORY: &str = ".nanocodex/eval-diff";
const DEFAULT_INITIAL_GUEST_MEMORY_MB: u64 = 512;
const MEMORY_PROFILE_FILE: &str = "differential-memory-profiles.json";

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum ToolMode {
    CodeMode,
    #[default]
    CodeModeOnly,
}

/// One explicit matched differential pair.
///
/// Profile-wide matrices are driven by the benchmark agent through repeated
/// `eval run --task` commands and the durable SQLite ledger.
#[derive(Args)]
pub(crate) struct Diff {
    /// Rebuild analysis from one retained comparison without model or VM work.
    #[arg(long, value_name = "COMPARISON_DIRECTORY", conflicts_with = "task")]
    reanalyze: Option<PathBuf>,

    /// Exact native task package for one matched pair.
    #[arg(
        long,
        value_name = "DIRECTORY",
        required_unless_present = "reanalyze",
        conflicts_with = "reanalyze"
    )]
    task: Option<PathBuf>,

    /// Exact stock-Codex Linux executable run in the guest.
    #[arg(
        long,
        value_name = "EXECUTABLE",
        required_unless_present = "reanalyze",
        conflicts_with = "reanalyze"
    )]
    codex_bin: Option<PathBuf>,

    /// Stock-Codex model-visible tool exposure.
    #[arg(long, value_enum, default_value_t)]
    codex_tool_mode: ToolMode,

    /// Nanocodex model-visible tool exposure.
    #[arg(long, value_enum, default_value_t)]
    nanocodex_tool_mode: ToolMode,

    /// Parent directory for this one retained matched pair.
    #[arg(long, default_value = DEFAULT_OUTPUT_DIRECTORY)]
    output: PathBuf,

    /// Initial eval-only guest RAM allocated to each arm.
    #[arg(
        long,
        value_name = "MIB",
        default_value_t = DEFAULT_INITIAL_GUEST_MEMORY_MB,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    guest_memory_mb: u64,

    #[command(flatten)]
    vm: VmPreparationArgs,

    /// Print the retained pair record as JSON.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    observability: ObservabilityArgs,

    #[command(flatten)]
    agent: EvalAgentArgs,
}

impl Diff {
    pub(crate) async fn run(self) -> Result<()> {
        let _observability = self.observability.install(false, Path::new("."))?;
        if let Some(directory) = self.reanalyze {
            let reanalysis = reanalyze(directory)?;
            if self.json {
                write_json(reanalysis.comparison())?;
            } else {
                print!("{}", reanalysis.human_summary());
            }
            return Ok(());
        }

        let task = Task::load(
            self.task
                .ok_or_else(|| eyre!("--task is required unless --reanalyze is used"))?,
        )?;
        let thinking = self.agent.thinking().unwrap_or_default();
        let web_search = self.agent.web_search().unwrap_or(false);
        let (nanocodex, auth) =
            self.agent
                .shared_builder(Model::default(), thinking, web_search)?;
        let codex_auth = match auth {
            SharedAuth::ApiKey(api_key) => CodexAuth::api_key(api_key),
            SharedAuth::AuthFile(path) => CodexAuth::auth_file(path),
        };
        let current_executable = std::env::current_exe()?;
        let runtime_image = run::prepare_vm_guest_runtime_from(
            self.vm.vm_guest_runtime.as_deref(),
            &self.vm.vm_cache,
        )
        .await?;
        let resources = VmResources::builder(&current_executable, runtime_image)
            .task(task.clone())
            .cache_directory(&self.vm.vm_cache)
            .cache_policy(if self.vm.vm_refresh {
                CachePolicy::Refresh
            } else {
                CachePolicy::Reuse
            })
            .image_preparation_concurrency(1)
            .prepare()
            .await?;
        let evaluator = DifferentialEvaluator::builder(nanocodex)
            .codex(
                self.codex_bin
                    .ok_or_else(|| eyre!("--codex-bin is required unless --reanalyze is used"))?,
                codex_auth,
            )
            .vm(resources)
            .output_directory(&self.output)
            .thinking(thinking)
            .web_search(web_search)
            .nanocodex_tool_mode(self.nanocodex_tool_mode.into())
            .codex_tool_mode(self.codex_tool_mode.into())
            .nanocodex_executable(
                ExecutableIdentity::new(current_executable, env!("NANOCODEX_SEMVER_VERSION"))
                    .git_sha(env!("VERGEN_GIT_SHA"))
                    .built_at(env!("VERGEN_BUILD_TIMESTAMP")),
            )
            .initial_guest_memory_mb(self.guest_memory_mb)
            .memory_profile_path(self.vm.vm_cache.join(MEMORY_PROFILE_FILE))
            .max_concurrency(1)
            .prepare()
            .await?;
        let report = evaluator.task(task).await?;
        if self.json {
            write_json(&report)?;
        } else {
            print!("{}", report.human_summary());
        }
        if report.has_infrastructure_failure() || report.has_operational_error() {
            return Err(eyre!(
                "matched pair retained retryable infrastructure evidence at {}",
                report.comparison_path().display()
            ));
        }
        Ok(())
    }
}

fn write_json(value: &impl serde::Serialize) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

impl From<ToolMode> for CodexToolMode {
    fn from(value: ToolMode) -> Self {
        match value {
            ToolMode::CodeMode => Self::CodeMode,
            ToolMode::CodeModeOnly => Self::CodeModeOnly,
        }
    }
}

impl From<ToolMode> for NanocodexToolMode {
    fn from(value: ToolMode) -> Self {
        match value {
            ToolMode::CodeMode => Self::CodeMode,
            ToolMode::CodeModeOnly => Self::CodeModeOnly,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::Diff;
    use crate::{Cli, Command, eval::EvalCommand};

    #[test]
    fn differential_cli_accepts_exactly_one_task_and_no_scheduler_knobs() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "diff",
            "--task",
            "/tmp/task",
            "--codex-bin",
            "/tmp/codex",
            "--api-key",
            "test-key",
        ])
        .unwrap();
        let Some(Command::Eval(eval)) = cli.command else {
            panic!("expected eval command");
        };
        assert!(matches!(eval.command, EvalCommand::Diff(Diff { .. })));

        assert!(
            Cli::try_parse_from([
                "nanocodex",
                "eval",
                "diff",
                "--task",
                "/tmp/task",
                "--codex-bin",
                "/tmp/codex",
                "--concurrency",
                "8",
            ])
            .is_err()
        );
    }

    #[test]
    fn reanalysis_needs_no_agent_or_scheduler_configuration() {
        Cli::try_parse_from([
            "nanocodex",
            "eval",
            "diff",
            "--reanalyze",
            "/tmp/comparison",
        ])
        .unwrap();
    }
}
