mod args;
mod benchmark;
mod cleanup;
mod diff;
mod inspect;
mod profile;
mod run;

use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use clap::{Args, Subcommand};
use eyre::{Result, eyre};
use nanocodex_eval::{
    Task, VerifierCollect, VerifierEnvironmentMode,
    vm::{prepare_task_image, prepare_verifier_image},
};
use nanocodex_vm::image::{CachePolicy, DiskStatus};
use serde::Serialize;

#[derive(Args)]
pub(crate) struct Eval {
    #[command(subcommand)]
    command: EvalCommand,
}

#[derive(Subcommand)]
enum EvalCommand {
    /// Launch the agent-owned benchmark workflow in the TUI or headlessly.
    Benchmark(benchmark::Benchmark),

    /// Initialize every desired coordinate in the durable profile ledger.
    Init(profile::Init),

    /// Inspect one immutable profile revision and its durable progress.
    Status(profile::Status),

    /// Durably execute one agent-selected task repetition from a profile.
    Run(profile::Run),

    /// Prepare task VM images without running agents.
    Prepare(Prepare),

    /// Load, validate, and inspect a benchmark task directory.
    Task {
        /// Directory containing task.toml, instruction.md, and tests/test.sh.
        directory: PathBuf,

        /// Emit the complete loaded task as JSON.
        #[arg(long)]
        json: bool,

        /// Include the complete prompt in human-readable output.
        #[arg(long, conflicts_with = "json")]
        prompt: bool,
    },

    /// Explain a retained Harbor job or trial and surface exact failure evidence.
    Inspect(inspect::Inspect),

    /// Run one matched Nanocodex and pinned stock-Codex task pair.
    Diff(diff::Diff),

    /// Remove disposable VM disks from completed retained trials.
    Cleanup(cleanup::Cleanup),
}

#[derive(Args)]
struct Prepare {
    /// Terminal-Bench task directory. Repeat to prepare several environments.
    #[arg(
        long = "task",
        value_name = "DIRECTORY",
        required_unless_present = "suites"
    )]
    tasks: Vec<PathBuf>,

    /// Terminal-Bench suite directory whose immediate task children should prepare.
    #[arg(
        long = "suite",
        value_name = "DIRECTORY",
        required_unless_present = "tasks"
    )]
    suites: Vec<PathBuf>,

    /// Content-addressed VM cache directory.
    #[arg(long, default_value = ".cache/vm")]
    cache: PathBuf,

    /// Resolve image references at the registry even when locally cached.
    #[arg(long)]
    refresh: bool,
}

impl Eval {
    pub(crate) async fn run(self) -> Result<()> {
        enable_paint();
        run(self).await
    }
}

fn enable_paint() {
    let enable = yansi::Condition::os_support() && yansi::Condition::tty_and_color_live();
    yansi::whenever(yansi::Condition::cached(enable));
}

async fn prepare_tasks(
    tasks: Vec<PathBuf>,
    suites: Vec<PathBuf>,
    cache: PathBuf,
    refresh: bool,
) -> Result<()> {
    let preparation_started = Instant::now();
    let tasks = run::load_task_paths(tasks, suites)?
        .into_iter()
        .map(Task::load)
        .collect::<Result<Vec<_>, _>>()?;
    let policy = if refresh {
        CachePolicy::Refresh
    } else {
        CachePolicy::Reuse
    };
    // Resolve the running, entitled VMM executable before a nested guest
    // runtime build can cause Cargo's runner cache to rotate paths.
    let vmm = std::env::current_exe()?;
    let runtime_started = Instant::now();
    let runtime_image = run::prepare_vm_guest_runtime().await?;
    let runtime_duration = runtime_started.elapsed();
    let builder = nanocodex_eval::vm::image_builder(&vmm, &runtime_image);
    let mut cache_hits = 0_usize;
    let mut cache_creations = 0_usize;
    let mut failures = Vec::new();
    for task in tasks {
        let task_started = Instant::now();
        if let Err(error) = task.validate_package() {
            eprintln!(
                "{}: task package changed duration={:.3?}\n{error:#}",
                task.name(),
                task_started.elapsed()
            );
            failures.push(task.name().to_owned());
            continue;
        }
        let prepared = match prepare_task_image(&builder, &task, &cache, policy).await {
            Ok(prepared) => prepared,
            Err(error) => {
                eprintln!(
                    "{}: failed duration={:.3?}\n{error:#}",
                    task.name(),
                    task_started.elapsed()
                );
                failures.push(task.name().to_owned());
                continue;
            }
        };
        if let Err(error) = task.validate_package() {
            eprintln!(
                "{}: task package changed during image preparation duration={:.3?}\n{error:#}",
                task.name(),
                task_started.elapsed()
            );
            failures.push(task.name().to_owned());
            continue;
        }
        match prepared.disk_status() {
            DiskStatus::Hit => cache_hits += 1,
            DiskStatus::Created => cache_creations += 1,
        }
        eprintln!(
            "{}: manifest={} ({}) root_disk={} duration={:.3?}",
            task.name(),
            prepared.manifest_digest(),
            prepared.manifest_source().as_str(),
            prepared.disk_status().as_str(),
            task_started.elapsed()
        );
        println!("{}", prepared.path().display());
        if task.verifier().environment_mode() == VerifierEnvironmentMode::Separate {
            let verifier_started = Instant::now();
            let verifier = match prepare_verifier_image(&builder, &task, &cache, policy).await {
                Ok(verifier) => verifier,
                Err(error) => {
                    eprintln!(
                        "{} verifier: failed duration={:.3?}\n{error:#}",
                        task.name(),
                        verifier_started.elapsed()
                    );
                    failures.push(format!("{} verifier", task.name()));
                    continue;
                }
            };
            if let Err(error) = task.validate_package() {
                eprintln!(
                    "{} verifier: task package changed during image preparation duration={:.3?}\n{error:#}",
                    task.name(),
                    verifier_started.elapsed()
                );
                failures.push(format!("{} verifier", task.name()));
                continue;
            }
            match verifier.disk_status() {
                DiskStatus::Hit => cache_hits += 1,
                DiskStatus::Created => cache_creations += 1,
            }
            eprintln!(
                "{} verifier: manifest={} ({}) root_disk={} duration={:.3?}",
                task.name(),
                verifier.manifest_digest(),
                verifier.manifest_source().as_str(),
                verifier.disk_status().as_str(),
                verifier_started.elapsed()
            );
            println!("{}", verifier.path().display());
        }
    }
    eprintln!(
        "VM preparation: runtime={runtime_duration:.3?} environments={} hits={cache_hits} created={cache_creations} failed={} total={:.3?}",
        cache_hits + cache_creations,
        failures.len(),
        preparation_started.elapsed()
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(eyre!(
            "{} VM environment(s) failed preparation: {}",
            failures.len(),
            failures.join(", ")
        ))
    }
}

async fn run(eval: Eval) -> Result<()> {
    match eval.command {
        EvalCommand::Benchmark(command) => command.run().await?,
        EvalCommand::Init(command) => command.run()?,
        EvalCommand::Status(command) => command.run()?,
        EvalCommand::Run(command) => command.run().await?,
        EvalCommand::Prepare(Prepare {
            tasks,
            suites,
            cache,
            refresh,
        }) => prepare_tasks(tasks, suites, cache, refresh).await?,
        EvalCommand::Task {
            directory,
            json,
            prompt,
        } => {
            let task = Task::load(directory)?;
            let output = TaskOutput::from(&task);
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            if json {
                serde_json::to_writer_pretty(&mut stdout, &output)?;
                writeln!(stdout)?;
            } else {
                output.write_human(&mut stdout, prompt)?;
            }
        }
        EvalCommand::Inspect(command) => command.run()?,
        EvalCommand::Diff(command) => command.run().await?,
        EvalCommand::Cleanup(command) => command.run()?,
    }
    Ok(())
}

#[derive(Serialize)]
struct TaskOutput<'a> {
    name: &'a str,
    description: &'a str,
    root: &'a Path,
    prompt: &'a str,
    image: &'a str,
    agent_timeout_sec: f64,
    verifier: VerifierOutput<'a>,
    artifacts: &'a [PathBuf],
    resources: ResourcesOutput,
    network: &'static str,
    environment: &'a BTreeMap<String, String>,
    requires_compose: bool,
}

#[derive(Serialize)]
struct VerifierOutput<'a> {
    script: &'a Path,
    timeout_sec: f64,
    environment: &'a BTreeMap<String, String>,
    environment_mode: VerifierEnvironmentMode,
    collect: &'a [VerifierCollect],
}

#[derive(Serialize)]
struct ResourcesOutput {
    cpus: u32,
    memory_mb: u64,
    storage_mb: u64,
    gpus: u32,
}

impl<'a> From<&'a Task> for TaskOutput<'a> {
    fn from(task: &'a Task) -> Self {
        Self {
            name: task.name(),
            description: task.description(),
            root: task.root(),
            prompt: task.prompt(),
            image: task.image().reference(),
            agent_timeout_sec: task.agent_timeout().as_secs_f64(),
            verifier: VerifierOutput {
                script: task.verifier().script(),
                timeout_sec: task.verifier().timeout().as_secs_f64(),
                environment: task.verifier().environment(),
                environment_mode: task.verifier().environment_mode(),
                collect: task.verifier().collect(),
            },
            artifacts: task.artifacts(),
            resources: ResourcesOutput {
                cpus: task.resources().cpus,
                memory_mb: task.resources().memory_mb,
                storage_mb: task.resources().storage_mb,
                gpus: task.resources().gpus,
            },
            network: task.network().as_str(),
            environment: task.environment(),
            requires_compose: task.requires_compose(),
        }
    }
}

impl TaskOutput<'_> {
    fn write_human(&self, mut output: impl Write, include_prompt: bool) -> io::Result<()> {
        writeln!(output, "{}", self.name)?;
        writeln!(output, "  root: {}", self.root.display())?;
        writeln!(output, "  image: {}", self.image)?;
        writeln!(output, "  prompt: {} bytes", self.prompt.len())?;
        writeln!(
            output,
            "  timeout: {}s agent, {}s verifier",
            self.agent_timeout_sec, self.verifier.timeout_sec
        )?;
        writeln!(
            output,
            "  resources: {} CPU, {} MiB memory, {} MiB storage, {} GPU",
            self.resources.cpus,
            self.resources.memory_mb,
            self.resources.storage_mb,
            self.resources.gpus
        )?;
        writeln!(output, "  network: {}", self.network)?;
        writeln!(
            output,
            "  verifier: {} ({})",
            self.verifier.script.display(),
            self.verifier.environment_mode.as_str()
        )?;
        writeln!(output, "  artifacts: {}", self.artifacts.len())?;
        writeln!(output, "  requires compose: {}", self.requires_compose)?;
        if include_prompt {
            writeln!(output, "\n{}", self.prompt)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::{CommandFactory, Parser};

    use super::{Eval, EvalCommand};
    use crate::{Cli, Command};

    #[test]
    fn prepare_accepts_repeated_tasks_in_input_order() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "prepare",
            "--task",
            "tasks/first",
            "--task",
            "tasks/second",
        ])
        .unwrap();
        let Some(Command::Eval(Eval {
            command: EvalCommand::Prepare(super::Prepare { tasks, .. }),
        })) = cli.command
        else {
            panic!("expected vm prepare command");
        };

        assert_eq!(
            tasks,
            [
                Path::new("tasks/first").to_path_buf(),
                Path::new("tasks/second").to_path_buf()
            ]
        );
    }

    #[test]
    fn prepare_accepts_a_complete_suite() {
        let cli = Cli::try_parse_from([
            "nanocodex",
            "eval",
            "prepare",
            "--suite",
            "terminal-bench-2-1",
        ])
        .unwrap();
        let Some(Command::Eval(Eval {
            command: EvalCommand::Prepare(super::Prepare { suites, .. }),
        })) = cli.command
        else {
            panic!("expected vm prepare command");
        };

        assert_eq!(suites, [Path::new("terminal-bench-2-1").to_path_buf()]);
    }

    #[test]
    fn complete_eval_surface_is_nested_under_nanocodex() {
        for arguments in [
            vec!["nanocodex", "eval", "init", "local-smoke"],
            vec![
                "nanocodex",
                "eval",
                "run",
                "local-smoke",
                "--task",
                "tasks/write-greeting",
                "--vm-cache",
                "/var/cache/nanocodex-vm",
            ],
            vec!["nanocodex", "eval", "status", "local-smoke"],
            vec!["nanocodex", "eval", "benchmark", "local-smoke"],
            vec![
                "nanocodex",
                "eval",
                "task",
                "tasks/write-greeting",
                "--json",
            ],
            vec!["nanocodex", "eval", "inspect", ".nanocodex/evals/job"],
            vec![
                "nanocodex",
                "eval",
                "diff",
                "--task",
                "tasks/write-greeting",
                "--codex-bin",
                "/tmp/codex",
                "--vm-cache",
                "/var/cache/nanocodex-vm",
                "--thinking",
                "medium",
            ],
            vec![
                "nanocodex",
                "eval",
                "cleanup",
                ".nanocodex/evals",
                "--dry-run",
            ],
        ] {
            Cli::try_parse_from(arguments).expect("supported eval command must parse");
        }

        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("eval"));
    }
}
