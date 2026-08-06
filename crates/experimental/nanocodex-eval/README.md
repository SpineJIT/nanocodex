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

## Durable API

```rust,no_run
use std::time::Duration;
use nanocodex_eval::{Evaluation, EvaluationClaim, EvaluationSelector};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let evaluation = Evaluation::open(
    "nanocodex.toml",
    Some("local-smoke"),
    ".nanocodex/evals",
)?;
let selector = EvaluationSelector::new("tasks/write-greeting");
match evaluation.claim(&selector, Duration::from_secs(300))? {
    EvaluationClaim::Prepare(claim) => {
        // Prepare `claim.task()`, then atomically accept or retry the lease.
        claim.complete()?;
    }
    EvaluationClaim::Run(claim) => {
        // Execute `claim.task()` with `claim.treatment()` and retain output in
        // `claim.output_directory()`, then accept or retry the result.
        let evidence = claim.output_directory().to_path_buf();
        claim.complete(&evidence)?;
    }
    EvaluationClaim::Busy(_) | EvaluationClaim::Complete => {}
}
# Ok(())
# }
```

Claims own lease heartbeats and fenced completion. Raw SQLite worksets, lease
generations, and artifact-coordinate construction are private implementation
details.

## Profiles

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

`Evaluation::open` resolves task packages, fingerprints their complete
execution inputs, and materializes every desired repetition in SQLite before
execution begins. Differential profiles also fingerprint the pinned
stock-Codex executable.

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

Set `NANOCODEX_BIN` and `NANOCODEX_VM_RUNTIME` when the default development
paths do not apply.
