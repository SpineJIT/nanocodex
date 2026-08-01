use std::{
    io::{self, Write as _},
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use clap::{Args, ValueEnum};
use eyre::{Result, eyre};
use nanocodex::Thinking;
use nanocodex_eval::{
    differential::{NanocodexToolMode as EvalNanocodexToolMode, *},
    vm::{CachePolicy, VmResources},
};

use super::{
    args::{SchedulingArgs, VmPreparationArgs},
    run,
};
use crate::{
    config::{EvalAgentArgs, SharedAuth},
    observability::ObservabilityArgs,
};

const DEFAULT_OUTPUT_DIRECTORY: &str = ".nanocodex/eval-diff";
const DEFAULT_INITIAL_GUEST_MEMORY_MB: u64 = 512;
const MEMORY_PROFILE_FILE: &str = "differential-memory-profiles.json";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum StockCodexToolMode {
    /// Expose normal tools directly as well as through Code Mode.
    CodeMode,
    /// Expose normal tools only through Code Mode's `exec` entrypoint.
    #[default]
    CodeModeOnly,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum NanocodexToolModeArg {
    /// Expose normal tools directly as well as through Code Mode.
    CodeMode,
    /// Expose normal tools only through Code Mode's `exec` entrypoint.
    #[default]
    CodeModeOnly,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct DifferentialScoreSummary {
    attempts: usize,
    valid: usize,
    infrastructure: usize,
    incomplete: usize,
    nanocodex_passes: usize,
    codex_passes: usize,
}

impl DifferentialScoreSummary {
    const fn observe(
        &mut self,
        classification: DifferentialClassification,
        infrastructure: bool,
        operational_error: bool,
    ) {
        self.attempts += 1;
        if infrastructure {
            self.infrastructure += 1;
            return;
        }
        if operational_error {
            self.incomplete += 1;
            return;
        }
        match classification {
            DifferentialClassification::BothPassed => {
                self.valid += 1;
                self.nanocodex_passes += 1;
                self.codex_passes += 1;
            }
            DifferentialClassification::CodexOnlyPassed => {
                self.valid += 1;
                self.codex_passes += 1;
            }
            DifferentialClassification::NanocodexOnlyPassed => {
                self.valid += 1;
                self.nanocodex_passes += 1;
            }
            DifferentialClassification::NeitherPassed => {
                self.valid += 1;
            }
            DifferentialClassification::Incomplete => {
                self.incomplete += 1;
            }
        }
    }
}

impl From<StockCodexToolMode> for CodexToolMode {
    fn from(value: StockCodexToolMode) -> Self {
        match value {
            StockCodexToolMode::CodeMode => Self::CodeMode,
            StockCodexToolMode::CodeModeOnly => Self::CodeModeOnly,
        }
    }
}

impl From<NanocodexToolModeArg> for EvalNanocodexToolMode {
    fn from(value: NanocodexToolModeArg) -> Self {
        match value {
            NanocodexToolModeArg::CodeMode => Self::CodeMode,
            NanocodexToolModeArg::CodeModeOnly => Self::CodeModeOnly,
        }
    }
}

#[derive(Args)]
pub(crate) struct Diff {
    /// Rebuild the derived API/event-loop comparison from retained raw captures.
    ///
    /// This performs no agent, model, VM, or verifier work.
    #[arg(
        long,
        value_name = "COMPARISON_DIRECTORY",
        conflicts_with_all = [
            "tasks",
            "suites",
            "codex_bin",
            "vm_cache",
            "vm_guest_runtime",
            "vm_refresh"
        ]
    )]
    reanalyze: Option<PathBuf>,

    /// Evaluator task directory to run through both agents. Repeat for a batch.
    #[arg(
        long = "task",
        value_name = "DIRECTORY",
        required_unless_present_any = ["reanalyze", "suites"],
        conflicts_with = "reanalyze"
    )]
    tasks: Vec<PathBuf>,

    /// Suite whose immediate task children should run through both agents.
    #[arg(
        long = "suite",
        value_name = "DIRECTORY",
        required_unless_present_any = ["reanalyze", "tasks"],
        conflicts_with = "reanalyze"
    )]
    suites: Vec<PathBuf>,

    /// Exact stock-Codex executable to compare against Nanocodex.
    ///
    /// This must be the Linux executable that runs in the guest.
    #[arg(
        long,
        value_name = "EXECUTABLE",
        required_unless_present = "reanalyze",
        conflicts_with = "reanalyze"
    )]
    codex_bin: Option<PathBuf>,

    /// Stock Codex tool exposure profiles included in this sweep.
    ///
    /// Repeat the flag or use a comma-delimited value to schedule several
    /// profiles through one host-wide queue.
    #[arg(
        long = "codex-tool-mode",
        value_enum,
        value_delimiter = ',',
        default_value = "code-mode-only"
    )]
    codex_tool_modes: Vec<StockCodexToolMode>,

    /// Nanocodex tool exposure profiles included in this sweep.
    ///
    /// Nanocodex and stock-Codex mode lists pair positionally. A singleton on
    /// either side is broadcast across the other list.
    #[arg(
        long = "nanocodex-tool-mode",
        value_enum,
        value_delimiter = ',',
        default_value = "code-mode-only"
    )]
    nanocodex_tool_modes: Vec<NanocodexToolModeArg>,

    /// Reasoning-effort profiles included in this sweep.
    ///
    /// Repeat the flag or use a comma-delimited value. When omitted, the
    /// shared `--thinking` value (or medium) supplies the sole effort.
    #[arg(long = "thinking-profile", value_delimiter = ',')]
    thinking_profiles: Vec<Thinking>,

    /// Parent directory for paired evaluator artifacts.
    #[arg(long, default_value = DEFAULT_OUTPUT_DIRECTORY)]
    output: PathBuf,

    #[command(flatten)]
    scheduling: SchedulingArgs,

    /// Maximum number of cold task images prepared concurrently.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    prepare_concurrency: Option<u16>,

    /// Initial eval-only guest RAM allocated to each arm before calibration.
    ///
    /// Tasks declaring less retain their smaller allocation. Confirmed OOMs
    /// grow this value; successful runs persist measured sizing with slack.
    #[arg(long, value_name = "MIB", value_parser = clap::value_parser!(u64).range(1..))]
    guest_memory_mb: Option<u64>,

    #[command(flatten)]
    vm: VmPreparationArgs,

    /// Print the complete sweep record as JSON.
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
        run::raise_eval_open_file_limit()?;

        let tasks = run::load_tasks(self.tasks, self.suites)?;
        let trials = NonZeroUsize::new(usize::from(self.scheduling.trials))
            .ok_or_else(|| eyre!("--trials must be greater than zero"))?;
        let requested_trials = trials.get();
        let nanocodex_tool_modes = resolve_nanocodex_tool_modes(&self.nanocodex_tool_modes)?;
        let codex_tool_modes = resolve_codex_tool_modes(&self.codex_tool_modes)?;
        let tool_mode_pairs = resolve_tool_mode_pairs(&nanocodex_tool_modes, &codex_tool_modes)?;
        let thinking_profiles =
            resolve_thinking_profiles(self.agent.thinking(), &self.thinking_profiles)?;
        let profiles = resolve_differential_profiles(&thinking_profiles, &tool_mode_pairs);
        let primary_profile = profiles
            .first()
            .copied()
            .ok_or_else(|| eyre!("at least one differential profile is required"))?;
        let task_names = tasks
            .iter()
            .map(|task| task.name().to_owned())
            .collect::<Vec<_>>();
        let (automatic_concurrency, automatic_memory_mb) =
            run::automatic_scheduling_defaults(self.scheduling.host_utilization);
        let concurrency = self.scheduling.concurrency.unwrap_or(automatic_concurrency);
        let prepare_concurrency = self
            .prepare_concurrency
            .unwrap_or_else(|| concurrency.clamp(1, 8));
        let max_memory_mb = self.scheduling.max_memory_mb.or(automatic_memory_mb);
        let initial_guest_memory_mb = self
            .guest_memory_mb
            .unwrap_or(DEFAULT_INITIAL_GUEST_MEMORY_MB);
        eprintln!(
            "Differential sweep: {} task(s) × {} profile(s) × k={} · up to {} pair(s) · {} · \
             {initial_guest_memory_mb} MiB initial per-arm guest RAM",
            tasks.len(),
            profiles.len(),
            self.scheduling.trials,
            concurrency,
            max_memory_mb.map_or_else(
                || "unbounded measured host memory".to_owned(),
                |memory| format!("{memory} MiB measured host-memory target")
            )
        );
        let thinking = primary_profile.thinking();
        let web_search = self.agent.web_search().unwrap_or(false);
        let (nanocodex, auth) = self.agent.shared_builder(thinking, web_search)?;
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
        let vm = VmResources::builder(&current_executable, runtime_image)
            .tasks(tasks.clone())
            .cache_directory(&self.vm.vm_cache)
            .cache_policy(if self.vm.vm_refresh {
                CachePolicy::Refresh
            } else {
                CachePolicy::Reuse
            })
            .image_preparation_concurrency(usize::from(prepare_concurrency));
        let vm = vm.prepare().await?;
        let output = self.output;
        let mut evaluator = DifferentialEvaluator::builder(nanocodex)
            .codex(
                self.codex_bin
                    .ok_or_else(|| eyre!("--codex-bin is required unless --reanalyze is used"))?,
                codex_auth,
            )
            .vm(vm)
            .output_directory(&output)
            .thinking(thinking)
            .web_search(web_search)
            .nanocodex_tool_mode(primary_profile.nanocodex_tool_mode())
            .codex_tool_mode(primary_profile.codex_tool_mode())
            .nanocodex_executable(
                ExecutableIdentity::new(current_executable, env!("NANOCODEX_SEMVER_VERSION"))
                    .git_sha(env!("VERGEN_GIT_SHA"))
                    .built_at(env!("VERGEN_BUILD_TIMESTAMP")),
            )
            .initial_guest_memory_mb(initial_guest_memory_mb)
            .memory_profile_path(self.vm.vm_cache.join(MEMORY_PROFILE_FILE))
            .max_concurrency(usize::from(concurrency))
            .max_infrastructure_replacements(requested_trials);
        if let Some(max_memory_mb) = max_memory_mb {
            evaluator = evaluator.max_memory_mb(max_memory_mb);
        }
        let evaluator = evaluator.prepare().await?;
        let comparison_count = tasks
            .len()
            .saturating_mul(profiles.len())
            .saturating_mul(requested_trials);
        let interrupts = run::ctrl_c_interrupt()?;
        let execution = run::finish_or_drain(
            evaluator.tasks_n_with_profiles(tasks, trials, profiles.clone()),
            interrupts,
            comparison_count,
            || {
                let admitted = evaluator.begin_drain();
                eprintln!(
                    "Interrupt received; stopped admitting new comparisons after {admitted} \
                     pair(s), draining admitted work; press Ctrl-C again to abort"
                );
                admitted
            },
        )
        .await?;
        let run::DrainExecution {
            result,
            interrupted,
            interrupt,
            ..
        } = execution;
        run::finish_or_interrupt(
            async move {
                let sweep = match result {
                    Ok(sweep) => sweep,
                    Err(error) if interrupted => {
                        return Err(eyre!(
                            "differential sweep interrupted after draining admitted comparisons; \
                             queued comparisons were not started and retained evidence remains \
                             under {} ({error})",
                            output.display()
                        ));
                    }
                    Err(error) => return Err(error.into()),
                };
                write_score_summaries(&task_names, &profiles, requested_trials, sweep.summaries());
                if sweep.skipped() > 0 {
                    eprintln!(
                        "Differential resume: skipped {} already-valid pair(s)",
                        sweep.skipped()
                    );
                }
                if self.json {
                    write_json(&sweep)?;
                } else {
                    for report in sweep.reports() {
                        print!("{}", report.human_summary());
                    }
                }
                if interrupted {
                    return Err(eyre!(
                        "differential sweep interrupted after draining admitted comparisons; \
                         queued comparisons were not started and retained evidence remains under \
                         {}",
                        output.display()
                    ));
                }
                if let Some((task_name, profile, valid_pairs)) =
                    task_names.iter().find_map(|task_name| {
                        profiles.iter().find_map(|profile| {
                            let valid_pairs = sweep
                                .summaries()
                                .iter()
                                .filter(|summary| {
                                    summary.task_name() == task_name
                                        && summary.thinking() == profile.thinking().as_str()
                                        && summary.nanocodex_tool_mode()
                                            == profile.nanocodex_tool_mode()
                                        && summary.codex_tool_mode() == profile.codex_tool_mode()
                                        && !summary.has_infrastructure_failure()
                                        && !summary.has_operational_error()
                                })
                                .count();
                            (valid_pairs < requested_trials).then_some((
                                task_name,
                                *profile,
                                valid_pairs,
                            ))
                        })
                    })
                {
                    let evidence = sweep
                        .summaries()
                        .iter()
                        .find(|summary| {
                            summary.task_name() == task_name
                                && summary.thinking() == profile.thinking().as_str()
                                && summary.nanocodex_tool_mode()
                                    == profile.nanocodex_tool_mode()
                                && summary.codex_tool_mode() == profile.codex_tool_mode()
                                && (summary.has_infrastructure_failure()
                                    || summary.has_operational_error())
                        })
                        .map_or(output.as_path(), DifferentialReportSummary::comparison_path);
                    return Err(eyre!(
                        "task {task_name} profile {}/{}/{} retained {valid_pairs}/{requested_trials} \
                         valid matched pairs after bounded retries and replacements; evidence \
                         retained at {}",
                        profile.thinking().as_str(),
                        profile.nanocodex_tool_mode().as_str(),
                        profile.codex_tool_mode().as_str(),
                        evidence.display()
                    ));
                }
                Ok(())
            },
            interrupt,
        )
        .await??;
        Ok(())
    }
}

fn write_score_summaries(
    task_names: &[String],
    profiles: &[DifferentialProfile],
    requested_trials: usize,
    summaries: &[DifferentialReportSummary],
) {
    for task_name in task_names {
        for profile in profiles {
            let mut summary = DifferentialScoreSummary::default();
            for report in summaries.iter().filter(|report| {
                report.task_name() == task_name
                    && report.thinking() == profile.thinking().as_str()
                    && report.nanocodex_tool_mode() == profile.nanocodex_tool_mode()
                    && report.codex_tool_mode() == profile.codex_tool_mode()
            }) {
                summary.observe(
                    report.classification(),
                    report.has_infrastructure_failure(),
                    report.has_operational_error(),
                );
            }
            eprintln!(
                "Differential score: {task_name} · profile {}/{}/{} · valid {}/{} · attempts {} · \
                 infrastructure {} · incomplete {} · Nanocodex {}/{} · stock Codex {}/{}",
                profile.thinking().as_str(),
                profile.nanocodex_tool_mode().as_str(),
                profile.codex_tool_mode().as_str(),
                summary.valid,
                requested_trials,
                summary.attempts,
                summary.infrastructure,
                summary.incomplete,
                summary.nanocodex_passes,
                summary.valid,
                summary.codex_passes,
                summary.valid
            );
        }
    }
}

fn resolve_codex_tool_modes(requested: &[StockCodexToolMode]) -> Result<Vec<CodexToolMode>> {
    let mut resolved = Vec::with_capacity(requested.len());
    for tool_mode in requested.iter().copied().map(CodexToolMode::from) {
        if resolved.contains(&tool_mode) {
            return Err(eyre!(
                "duplicate --codex-tool-mode profile {}",
                tool_mode.as_str()
            ));
        }
        resolved.push(tool_mode);
    }
    if resolved.is_empty() {
        return Err(eyre!("at least one --codex-tool-mode is required"));
    }
    Ok(resolved)
}

fn resolve_nanocodex_tool_modes(
    requested: &[NanocodexToolModeArg],
) -> Result<Vec<EvalNanocodexToolMode>> {
    let mut resolved = Vec::with_capacity(requested.len());
    for tool_mode in requested.iter().copied().map(EvalNanocodexToolMode::from) {
        if resolved.contains(&tool_mode) {
            return Err(eyre!(
                "duplicate --nanocodex-tool-mode profile {}",
                tool_mode.as_str()
            ));
        }
        resolved.push(tool_mode);
    }
    if resolved.is_empty() {
        return Err(eyre!("at least one --nanocodex-tool-mode is required"));
    }
    Ok(resolved)
}

fn resolve_tool_mode_pairs(
    nanocodex: &[EvalNanocodexToolMode],
    codex: &[CodexToolMode],
) -> Result<Vec<(EvalNanocodexToolMode, CodexToolMode)>> {
    match (nanocodex.len(), codex.len()) {
        (0, _) | (_, 0) => Err(eyre!(
            "both Nanocodex and stock Codex require at least one tool-mode profile"
        )),
        (1, _) => Ok(codex
            .iter()
            .copied()
            .map(|codex| (nanocodex[0], codex))
            .collect()),
        (_, 1) => Ok(nanocodex
            .iter()
            .copied()
            .map(|nanocodex| (nanocodex, codex[0]))
            .collect()),
        (nanocodex_count, codex_count) if nanocodex_count == codex_count => Ok(nanocodex
            .iter()
            .copied()
            .zip(codex.iter().copied())
            .collect()),
        (nanocodex_count, codex_count) => Err(eyre!(
            "--nanocodex-tool-mode has {nanocodex_count} values but --codex-tool-mode has \
             {codex_count}; use equal-length lists or make either side a singleton"
        )),
    }
}

fn resolve_thinking_profiles(
    shared: Option<Thinking>,
    requested: &[Thinking],
) -> Result<Vec<Thinking>> {
    if requested.is_empty() {
        return Ok(vec![shared.unwrap_or(Thinking::Medium)]);
    }
    if shared.is_some() {
        return Err(eyre!(
            "--thinking and --thinking-profile cannot be used together"
        ));
    }
    let mut resolved = Vec::with_capacity(requested.len());
    for thinking in requested.iter().copied() {
        if resolved.contains(&thinking) {
            return Err(eyre!("duplicate --thinking-profile {}", thinking.as_str()));
        }
        resolved.push(thinking);
    }
    Ok(resolved)
}

fn resolve_differential_profiles(
    thinking_profiles: &[Thinking],
    tool_mode_pairs: &[(EvalNanocodexToolMode, CodexToolMode)],
) -> Vec<DifferentialProfile> {
    thinking_profiles
        .iter()
        .copied()
        .flat_map(|thinking| {
            tool_mode_pairs
                .iter()
                .copied()
                .map(move |(nanocodex_tool_mode, codex_tool_mode)| {
                    DifferentialProfile::new(thinking, nanocodex_tool_mode, codex_tool_mode)
                })
        })
        .collect()
}

fn write_json(value: &impl serde::Serialize) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;
    use nanocodex::Thinking;
    use nanocodex_eval::differential::{
        CodexToolMode, DifferentialClassification, NanocodexToolMode as EvalNanocodexToolMode,
    };

    use super::{
        Diff, DifferentialScoreSummary, NanocodexToolModeArg, StockCodexToolMode,
        resolve_codex_tool_modes, resolve_differential_profiles, resolve_nanocodex_tool_modes,
        resolve_thinking_profiles, resolve_tool_mode_pairs,
    };
    use crate::eval::args::DEFAULT_TRIALS;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        diff: Diff,
    }

    #[test]
    fn differential_score_summary_excludes_infrastructure_and_incomplete_pairs() {
        let mut summary = DifferentialScoreSummary::default();
        summary.observe(DifferentialClassification::BothPassed, false, false);
        summary.observe(DifferentialClassification::CodexOnlyPassed, false, false);
        summary.observe(
            DifferentialClassification::NanocodexOnlyPassed,
            false,
            false,
        );
        summary.observe(DifferentialClassification::NeitherPassed, false, false);
        summary.observe(DifferentialClassification::BothPassed, true, false);
        summary.observe(DifferentialClassification::BothPassed, false, true);
        summary.observe(DifferentialClassification::Incomplete, false, false);

        assert_eq!(
            summary,
            DifferentialScoreSummary {
                attempts: 7,
                valid: 4,
                infrastructure: 1,
                incomplete: 2,
                nanocodex_passes: 2,
                codex_passes: 2,
            }
        );
    }

    #[test]
    fn differential_cli_defaults_to_k_five() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--codex-bin",
            "/opt/codex",
        ])
        .unwrap();

        assert_eq!(cli.diff.scheduling.trials, DEFAULT_TRIALS);
        assert_eq!(cli.diff.scheduling.trials, 5);
    }

    #[test]
    fn differential_cli_accepts_batched_tasks_and_explicit_scheduler_limits() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--task",
            "tasks/second",
            "--suite",
            "tasks/suite",
            "--codex-bin",
            "/opt/codex",
            "--trials",
            "7",
            "--concurrency",
            "12",
            "--prepare-concurrency",
            "6",
            "--max-memory-mb",
            "49152",
            "--guest-memory-mb",
            "1024",
        ])
        .unwrap();

        assert_eq!(
            cli.diff.tasks,
            [
                Path::new("tasks/first").to_path_buf(),
                Path::new("tasks/second").to_path_buf()
            ]
        );
        assert_eq!(cli.diff.suites, [Path::new("tasks/suite").to_path_buf()]);
        assert_eq!(cli.diff.scheduling.trials, 7);
        assert_eq!(cli.diff.scheduling.concurrency, Some(12));
        assert_eq!(cli.diff.prepare_concurrency, Some(6));
        assert_eq!(cli.diff.scheduling.max_memory_mb, Some(49_152));
        assert_eq!(cli.diff.guest_memory_mb, Some(1_024));
        assert_eq!(
            cli.diff.codex_tool_modes,
            [StockCodexToolMode::CodeModeOnly]
        );
        assert_eq!(
            cli.diff.nanocodex_tool_modes,
            [NanocodexToolModeArg::CodeModeOnly]
        );
    }

    #[test]
    fn differential_cli_accepts_multiple_profiles_for_one_queue() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--codex-bin",
            "/opt/codex",
            "--codex-tool-mode",
            "code-mode,code-mode-only",
            "--thinking-profile",
            "low,high",
        ])
        .unwrap();

        assert_eq!(
            cli.diff.codex_tool_modes,
            [
                StockCodexToolMode::CodeMode,
                StockCodexToolMode::CodeModeOnly
            ]
        );
        assert_eq!(
            resolve_codex_tool_modes(&cli.diff.codex_tool_modes).unwrap(),
            [CodexToolMode::CodeMode, CodexToolMode::CodeModeOnly]
        );
        let thinking = resolve_thinking_profiles(None, &cli.diff.thinking_profiles).unwrap();
        assert_eq!(thinking, [Thinking::Low, Thinking::High]);
        let mode_pairs = resolve_tool_mode_pairs(
            &resolve_nanocodex_tool_modes(&cli.diff.nanocodex_tool_modes).unwrap(),
            &resolve_codex_tool_modes(&cli.diff.codex_tool_modes).unwrap(),
        )
        .unwrap();
        let profiles = resolve_differential_profiles(&thinking, &mode_pairs);
        assert_eq!(profiles.len(), 4);
        assert_eq!(profiles[0].thinking(), Thinking::Low);
        assert_eq!(
            profiles[0].nanocodex_tool_mode(),
            EvalNanocodexToolMode::CodeModeOnly
        );
        assert_eq!(profiles[0].codex_tool_mode(), CodexToolMode::CodeMode);
        assert_eq!(profiles[3].thinking(), Thinking::High);
        assert_eq!(
            profiles[3].nanocodex_tool_mode(),
            EvalNanocodexToolMode::CodeModeOnly
        );
        assert_eq!(profiles[3].codex_tool_mode(), CodexToolMode::CodeModeOnly);
    }

    #[test]
    fn differential_cli_pairs_both_agents_code_modes_positionally() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--codex-bin",
            "/opt/codex",
            "--nanocodex-tool-mode",
            "code-mode,code-mode-only",
            "--codex-tool-mode",
            "code-mode,code-mode-only",
        ])
        .unwrap();
        let mode_pairs = resolve_tool_mode_pairs(
            &resolve_nanocodex_tool_modes(&cli.diff.nanocodex_tool_modes).unwrap(),
            &resolve_codex_tool_modes(&cli.diff.codex_tool_modes).unwrap(),
        )
        .unwrap();

        assert_eq!(
            mode_pairs,
            [
                (EvalNanocodexToolMode::CodeMode, CodexToolMode::CodeMode),
                (
                    EvalNanocodexToolMode::CodeModeOnly,
                    CodexToolMode::CodeModeOnly
                ),
            ]
        );
        let profiles = resolve_differential_profiles(&[Thinking::High], &mode_pairs);
        assert_eq!(profiles.len(), 2, "each of the four arms runs exactly once");
    }

    #[test]
    fn differential_cli_rejects_ambiguous_tool_mode_pairing() {
        let error = resolve_tool_mode_pairs(
            &[
                EvalNanocodexToolMode::CodeMode,
                EvalNanocodexToolMode::CodeModeOnly,
            ],
            &[
                CodexToolMode::CodeMode,
                CodexToolMode::CodeModeOnly,
                CodexToolMode::CodeMode,
            ],
        )
        .unwrap_err();

        assert!(error.to_string().contains("equal-length lists"));
    }

    #[test]
    fn differential_cli_keeps_reanalysis_agent_free() {
        let cli =
            TestCli::try_parse_from(["nanoeval", "--reanalyze", "retained/comparison"]).unwrap();

        assert_eq!(
            cli.diff.reanalyze.as_deref(),
            Some(Path::new("retained/comparison"))
        );
        assert!(cli.diff.tasks.is_empty());
        assert!(cli.diff.codex_bin.is_none());
    }
}
