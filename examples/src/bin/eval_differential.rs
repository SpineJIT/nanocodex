#![recursion_limit = "256"]

use std::{env, path::PathBuf};

use nanocodex::{Nanocodex, OpenAi};
use nanocodex_eval::{
    Task,
    differential::{CodexAuth, DifferentialEvaluator, ExecutableIdentity},
};
use nanocodex_examples::eval_support as support;

#[tokio::main]
async fn main() -> Result<(), support::AnyError> {
    let task = Task::load(
        env::args_os()
            .nth(1)
            .map_or_else(|| PathBuf::from("tasks/write-greeting"), PathBuf::from),
    )?;
    let codex = env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .ok_or("usage: eval-differential TASK CODEX_LINUX_BINARY")?;
    let api_key = env::var("OPENAI_API_KEY")?;
    let nanocodex_bin = env::var_os("NANOCODEX_BIN")
        .map_or_else(|| PathBuf::from("target/debug/nanocodex"), PathBuf::from);
    let resources = support::vm_resources(vec![task.clone()]).await?;
    let evaluator =
        DifferentialEvaluator::builder(Nanocodex::builder(OpenAi::new(api_key.clone())?))
            .codex(codex, CodexAuth::api_key(api_key))
            .vm(resources)
            .nanocodex_executable(ExecutableIdentity::new(
                nanocodex_bin,
                env!("CARGO_PKG_VERSION"),
            ))
            .prepare()
            .await?;

    let report = evaluator.task(task).await?;
    println!(
        "retained matched comparison: {}",
        report.comparison_path().display()
    );
    Ok(())
}
