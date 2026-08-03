use std::{env, path::PathBuf};

use nanocodex::{Nanocodex, OpenAi, Thinking};
use nanocodex_eval::{Evaluator, Sweep, Task, harbor::Harbor};
use nanocodex_examples::eval_support as support;

#[tokio::main]
async fn main() -> Result<(), support::AnyError> {
    let task = Task::load(
        env::args_os()
            .nth(1)
            .map_or_else(|| PathBuf::from("tasks/write-greeting"), PathBuf::from),
    )?;
    let agent = Nanocodex::builder(OpenAi::new(support::auth()?)?);
    let sweep = Sweep::builder()
        .task(task.clone())
        .trials(3)
        .agent("low", agent.clone().thinking(Thinking::Low))?
        .agent("high", agent.clone().thinking(Thinking::High))?
        .build()?;
    let planned = sweep.attempt_count();
    let backend = support::vm_backend(vec![task]).await?;
    let evaluator = Evaluator::builder(agent, backend)
        .output_directory(".nanocodex/evals/examples")
        .max_concurrency(4)
        .resume_incomplete(sweep)
        .build()?;

    let remaining = evaluator.remaining_attempts()?;
    let run = evaluator.sweep();
    let recorder = Harbor::new(&evaluator)?.record(run.events().subscribe())?;
    let results = run.await?;
    let job = recorder.finish_all(remaining).await?;

    println!(
        "completed {}/{} attempts ({} resumed)",
        results.attempts().len(),
        planned,
        results.skipped()
    );
    println!("artifacts: {}", job.directory().display());
    Ok(())
}
