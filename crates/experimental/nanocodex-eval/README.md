# nanocodex-eval

`nanocodex-eval` owns Nanocodex's VM-isolated execution boundary and durable
profile ledger. A profile defines the complete desired task and treatment
matrix. Callers choose an exact family; SQLite allocates one internal
repetition and fences its accepted completion.

The ledger deliberately has no `next work`, `run all`, concurrency, or host
saturation policy. An embedding application or the `/benchmark` agent decides
which family to run and how many one-coordinate processes to launch.

Every benchmark attempt runs tools and verification in a microVM. Native host
execution exists only inside focused crate tests. Harbor JSONL and ATIF are
output formats, not alternate runners.

## One task

```rust,no_run
use nanocodex_agent::{Nanocodex, OpenAi};
use nanocodex_eval::{Evaluator, Task, VmResources};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let task = Task::load("tasks/write-greeting")?;
let resources = VmResources::builder("nanocodex", "runtime.ext4")
    .task(task.clone())
    .prepare()
    .await?;
let evaluator = Evaluator::builder(
    Nanocodex::builder(OpenAi::new(std::env::var("OPENAI_API_KEY")?)?),
    resources.backend().await?,
)
.output_directory(".nanocodex/evals")
.build()?;

let run = evaluator.task(task);
let mut events = run.events().subscribe();
let observer = tokio::spawn(async move {
    while let Some(event) = events.recv().await? {
        println!("{} {:?}", event.sequence, event.kind);
    }
    Ok::<_, nanocodex_eval::EvalEventStreamError>(())
});
let outcome = run.await?;
observer.await??;
println!("{:?}", outcome.outcome());
# Ok(())
# }
```

`EvalRun<T>` is independently awaitable and owns an optional event stream.
Every invocation emits one terminal event, including cancellation.

## Durable profiles

The repository manifest is `nanocodex.toml`:

```toml
default = "local-smoke"

[profiles.local-smoke]
tasks = ["tasks/write-greeting"]
trials = 3
model = ["sol"]
thinking = ["low"]
mode = "nanocodex"
```

`EvaluationManifest::load_profile` resolves task packages and fingerprints
their complete execution inputs. Differential profiles also fingerprint the
pinned stock-Codex executable. `Workset::ensure` then materializes every
desired repetition in SQLite before execution begins.

```text
profile -> exact task/treatment families -> k=1..N SQLite coordinates
                                      \
                                       -> callers choose families and fan-out
```

Task preparation is durable state too. One process owns a fenced preparation
lease while competing processes receive a temporary-unavailable result. A
coordinate completion is accepted only while its lease generation is current;
an expired worker cannot overwrite its replacement.

Leases guarantee exactly-once accepted completion, not absolutely
exactly-once model spending after a worker becomes unreachable. Heartbeats and
conservative expiry reduce duplicate spending; generation fencing prevents a
stale result from being committed.

## Differential evaluation

Detailed matched-pair APIs live under `nanocodex_eval::differential`; VM APIs
live under `nanocodex_eval::vm`. One differential coordinate runs its
Nanocodex and pinned stock-Codex arms concurrently, retains both trajectories
and verifier evidence, and publishes one atomic `comparison.json`.

Prepared task images and memory observations remain content-addressed cache
inputs. Each arm still receives a fresh writable overlay, so filesystem and
process state cannot leak between profile repetitions.

## CLI

```sh
# Materialize the complete closed profile and inspect exact counts.
nanocodex eval status local-smoke --json

# Execute one SQLite-assigned repetition from an exact profile task.
nanocodex eval run local-smoke --task tasks/write-greeting

# Let an agent inspect the ledger and choose task order and process fan-out.
nanocodex eval benchmark local-smoke
# Equivalent interactive workflow:
nanocodex
# then enter: /benchmark local-smoke
```

`--state-dir` overrides the default `~/.nanocodex/evals`. There is no trial
argument: `trials` is profile-owned desired work, and SQLite assigns a
fungible repetition inside the exact family selected by `--task` and any
needed model, thinking, or tool-mode selectors.

Compiled examples retain only the per-coordinate boundaries:

- `eval-task`: one VM attempt and its independent event stream.
- `eval-differential`: one matched Nanocodex-versus-Codex pair.

Set `NANOCODEX_BIN` and `NANOCODEX_VM_RUNTIME` when the default development
paths do not apply.
