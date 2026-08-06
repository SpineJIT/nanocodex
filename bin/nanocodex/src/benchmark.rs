use std::path::Path;

pub(crate) fn prompt(profile: Option<&str>, config: &Path, state_dir: Option<&Path>) -> String {
    let selected = profile.unwrap_or("the manifest default profile");
    let profile_argument =
        profile.map_or_else(String::new, |profile| format!(" {}", shell_quote(profile)));
    let state_argument = state_dir.map_or_else(String::new, |directory| {
        format!(" --state-dir {}", shell_quote(&directory.to_string_lossy()))
    });
    let config_argument = shell_quote(&config.to_string_lossy());
    format!(
        r#"Drive the closed Nanocodex evaluation profile {selected} to durable completion.

The desired amount of work is defined only by `{config}`. Never add an ad-hoc task, treatment, model, reasoning effort, or trial. Materialize and inspect its durable SQLite ledger with:

    nanocodex eval status{profile_argument} --config {config_argument}{state_argument} --json

You own execution strategy. Read the family records, choose an exact pending task and treatment, and invoke one repetition with `nanocodex eval run{profile_argument} --config {config_argument}{state_argument} --task <exact-profile-selector>` plus any model, thinking, or tool-mode selectors required to disambiguate that profile family. The CLI allocates the internal repetition; never pass or invent a trial number.

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
        );

        assert!(prompt.contains("choose an exact pending task and treatment"));
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
        );

        assert!(prompt.contains("status 'release candidate' --config 'configs/eval profile.toml'"));
        assert!(prompt.contains("--state-dir '/mnt/eval state'"));
    }
}
