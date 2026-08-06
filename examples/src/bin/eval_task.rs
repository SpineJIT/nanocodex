use std::{env, path::PathBuf};

use nanocodex::{Nanocodex, OpenAi};
use nanocodex_eval::{EvalEventKind, Evaluator, Task};
use nanocodex_examples::eval_support as support;

#[tokio::main]
async fn main() -> Result<(), support::AnyError> {
    let task = Task::load(
        env::args_os()
            .nth(1)
            .map_or_else(|| PathBuf::from("tasks/write-greeting"), PathBuf::from),
    )?;
    let backend = support::vm_backend(vec![task.clone()]).await?;
    let evaluator = Evaluator::builder(Nanocodex::builder(OpenAi::new(support::auth()?)?), backend)
        .output_directory(".nanocodex/evals/examples")
        .build()?;

    let run = evaluator.task(task);
    let mut events = run.events().subscribe();
    let observer = tokio::spawn(async move {
        while let Some(event) = events.recv().await? {
            if let EvalEventKind::Agent(agent) = &event.kind {
                eprintln!("{:?}", agent.kind);
            }
        }
        Ok::<_, nanocodex_eval::EvalEventStreamError>(())
    });

    let outcome = run.await?;
    observer.await??;
    println!("{}: {:?}", outcome.trial_name(), outcome.outcome());
    println!("artifacts: {}", evaluator.directory().display());
    Ok(())
}
