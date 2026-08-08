use std::path::Path;

pub(crate) fn prompt(
    profile: Option<&str>,
    config: &Path,
    state_dir: Option<&Path>,
    coordinator: Option<&str>,
    worker: Option<&str>,
    executable: Option<&Path>,
) -> String {
    let selected = profile.unwrap_or("the selected SQLite profile");
    let profile_argument =
        profile.map_or_else(String::new, |profile| format!(" {}", shell_quote(profile)));
    let state_argument = state_dir.map_or_else(String::new, |directory| {
        format!(" --state-dir {}", shell_quote(&directory.to_string_lossy()))
    });
    let coordinator_argument = coordinator.map_or_else(String::new, |coordinator| {
        format!(" --coordinator {}", shell_quote(coordinator))
    });
    let worker_argument = worker.map_or_else(String::new, |worker| {
        format!(" --worker {}", shell_quote(worker))
    });
    let config_argument = shell_quote(&config.to_string_lossy());
    let executable = executable.map_or_else(
        || "nanocodex".to_owned(),
        |executable| shell_quote(&executable.to_string_lossy()),
    );
    let ledger = if coordinator.is_some() {
        "the coordinator-backed SQLite ledger; do not open SQLite directly"
    } else {
        "SQLite"
    };
    format!(
        r#"Drive the Nanocodex evaluation profile {selected} to durable completion.

This is an operations loop, not a software-development task. Do not inspect repository source, plans, documentation, or configuration. Do not edit files. Begin with the status command below, then immediately launch a wave of pending work.

The desired amount of work is already materialized in {ledger}. Do not infer it from `{config}` or add ad-hoc work during this workflow. Inspect the durable ledger with:

    {executable} eval status{profile_argument}{state_argument}{coordinator_argument} --json --family-limit 32

You own execution strategy. Read the family records, choose an exact pending task and harness treatment, and invoke one repetition with `{executable} eval run{profile_argument} --config {config_argument}{state_argument}{coordinator_argument}{worker_argument} --task <exact-profile-selector>` plus `--harness`, model, or thinking selectors required to identify that profile family. Omit `--harness` for built-in Nanocodex. The CLI allocates the internal repetition; never pass or invent a trial number.

Decide how many run processes to launch concurrently and which tasks to prioritize. You may adjust fan-out based on memory, preparation contention, failures, and observed throughput. There is deliberately no run-all command, next-work command, scheduler, or host-saturation loop in the evaluator.

Task preparation is part of each task's durable state. One run process may prepare a task while another receives a temporary-unavailable result. Retry temporary contention after its suggested delay. Retry durable infrastructure failures, but treat accepted model and verifier outcomes as terminal even when the benchmark failed.

After each wave, inspect the ledger again. Continue until every desired coordinate is terminal or a concrete non-retryable blocker is established. Inspect retained evidence for infrastructure failures and representative accepted results. Do not modify Nanocodex source, benchmark tasks, verifier code, or expected outputs in this workflow. Finish with exact completed/running/pending counts, evidence locations, failures, exclusions, and any remaining blocker."#,
        config = config.display(),
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_leaves_task_choice_and_parallelism_to_the_agent() {
        let prompt = prompt(
            Some("release"),
            Path::new("nanocodex.toml"),
            Some(Path::new("/mnt/evals")),
            None,
            None,
            None,
        );

        assert!(prompt.contains("choose an exact pending task and harness treatment"));
        assert!(prompt.contains("Omit `--harness` for built-in Nanocodex"));
        assert!(prompt.contains("Decide how many run processes to launch concurrently"));
        assert!(prompt.contains("never pass or invent a trial number"));
        assert!(prompt.contains("--state-dir '/mnt/evals'"));
        assert!(!prompt.contains("eval work"));
    }

    #[test]
    fn workflow_quotes_paths_and_profile_names_as_shell_arguments() {
        let prompt = prompt(
            Some("release candidate"),
            Path::new("configs/eval profile.toml"),
            Some(Path::new("/mnt/eval state")),
            None,
            None,
            None,
        );

        assert!(prompt.contains("status 'release candidate' --state-dir '/mnt/eval state'"));
        assert!(prompt.contains("--state-dir '/mnt/eval state'"));
    }

    #[test]
    fn workflow_can_point_every_run_at_a_coordinator() {
        let prompt = prompt(
            Some("release"),
            Path::new("nanocodex.toml"),
            None,
            Some("http://127.0.0.1:8789"),
            Some("dev-georgios-01"),
            Some(Path::new("/opt/nanocodex/bin/nanocodex")),
        );

        assert!(prompt.contains("status 'release' --coordinator 'http://127.0.0.1:8789'"));
        assert!(prompt.contains("--json --family-limit 32"));
        assert!(prompt.contains(
            "'/opt/nanocodex/bin/nanocodex' eval run 'release' --config 'nanocodex.toml' --coordinator 'http://127.0.0.1:8789' --worker 'dev-georgios-01' --task"
        ));
        assert!(prompt.contains("do not open SQLite directly"));
        assert!(prompt.contains("not a software-development task"));
        assert!(!prompt.contains("--state-dir"));
    }
}
