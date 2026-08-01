# nanocodex-eval

`nanocodex-eval` owns Nanocodex's VM-isolated benchmark lifecycle: task
loading, bounded scheduling, fresh attempts, canonical verification, resumable
jobs, typed events and outcomes, Harbor projection, aggregation, and matched
Codex comparison.

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
Every event carries a job ID, invocation ID, invocation-wide sequence, and—on
attempt events—typed attempt identity and ordering. Every invocation emits one
terminal event, including cancellation.

## Resumable sweep

Build a `Sweep`, then consume it exactly once with
`resume_incomplete(sweep)` or `fresh_run(sweep)`. The resulting evaluator is
bound to that manifest, so execution is simply `evaluator.sweep()` and cannot
accidentally receive a different workload.

See the compiled examples:

- `eval-task`: one VM attempt, independent events, and Harbor projection.
- `eval-sweep`: a resumable multi-agent sweep.
- `eval-differential`: matched Nanocodex-versus-Codex trials.

Set `NANOCODEX_BIN` and `NANOCODEX_VM_RUNTIME` when the default development
paths do not apply.

## Differential evaluation

Detailed comparison APIs live under `nanocodex_eval::differential`; detailed
VM APIs live under `nanocodex_eval::vm`. `DifferentialEvaluatorBuilder::prepare`
is async because it hashes and stages executables and loads retained memory
profiles before execution.

The scheduler is work-conserving across concurrency and memory limits. A task
larger than the memory target runs alone, differential arms release capacity
independently, image preparation overlaps across tasks, and draining stops new
admission while joining work already admitted.

### Execution topology

The unit of scheduling is a task lane. Different task lanes run in parallel,
up to the configured concurrency and measured host-memory limits. Coordinates
inside one lane run sequentially in profile and trial order. For a high-effort
`k=10` sweep whose profiles are code mode (CM) followed by code-mode-only
(CMO), the shape is:

```text
one host evaluator
│
├── task A lane ── prepare/reuse task image once
│   │
│   ├── CM1  ──┬── Nanocodex VM ──┐
│   │           └── stock Codex VM ├── atomic comparison.json
│   ├── CM2  ──┬── Nanocodex VM ──┤
│   │           └── stock Codex VM ┘
│   ├── ... sequentially through CM10
│   └── CMO1 ... CMO10 sequentially
│
├── task B lane ── same shape, running in parallel with task A
├── task C lane ── same shape, running in parallel when admitted
└── ...
```

The two arms of one coordinate run concurrently. Two coordinates for the same
task never overlap, so a task has at most one Nanocodex VM and one stock-Codex
VM live at once. This is a logical persistent lane, not a stateful persistent
guest: every arm gets a fresh writable overlay so filesystem or process state
cannot leak between benchmark trials.

### Disk and cache lifecycle

Prepared inputs are expensive and attempt mutations are disposable. Keep the
VM cache on durable storage and give each sweep its own output directory:

```text
NVMe
├── vm-cache/                         retained and shared across sweeps
│   ├── guest runtime
│   ├── content-addressed base images
│   ├── prepared task images
│   └── differential memory profiles
│
└── sweep-output/                     durable evidence for one exact manifest
    ├── differential-sweep.json
    ├── .differential-sweep.lock
    └── <task/profile/trial/id>/
        ├── progress.jsonl
        ├── nanocodex/.../*.upper.ext4  live writable overlay only
        ├── codex/.../*.upper.ext4      live writable overlay only
        ├── trajectories and verifier evidence
        └── comparison.json             atomic completion checkpoint
```

The immutable prepared image is reused; a writable `*.upper.ext4` contains only
one arm's changes. Normal completion, cancellation, and drop remove that
overlay. After a hard process or host failure, the next process acquires the
exclusive sweep lock and removes overlays from reportless interrupted
comparison directories before scheduling them again. Completed comparison
directories, diagnostic evidence, and shared cache entries are not removed.

### Exact resume behavior

`comparison.json` is the durable coordinate boundary and is published
atomically. Resume validates the sweep manifest, scans those checkpoints, and
filters the original ordered queue:

```text
before interruption
CM:  [1 ✓] [2 ✓] [3 ✓] [4 ✓] [5 ✓] [6 ✓] [7 interrupted] [8 pending] ... [10]
CMO: [1 pending] ......................................................... [10]

after restart
skip: CM1 ... CM6
run:  CM7 → CM8 → CM9 → CM10 → CMO1 → ... → CMO10
```

No VM checkpoint is restored. The interrupted coordinate starts in a fresh VM
from the reused prepared image; previously published coordinates do no model,
tool, VM, or verifier work again. A normal benchmark pass or failure is durable
evidence and is not retried merely because its score is zero. A missing report
is rerun at the same coordinate, a confirmed OOM retries that coordinate with
more memory, and retained infrastructure or runner failures use the configured
bounded replacement lineage.

Resume requires the same output directory and exact manifest identity: tasks
and their content digests, ordered profiles, trial count, model policy, and the
pinned Nanocodex and stock-Codex executables. Use a new output directory for a
different workload or binary build.

## CLI

```sh
nanocodex eval --suite /data/terminal-bench/tasks --trials 5
nanocodex eval diff \
  --suite /data/terminal-bench/tasks \
  --codex-bin /opt/codex/codex-x86_64-unknown-linux-musl
```

The CLI installs auth and observability, resolves reusable scheduling and VM
configuration, invokes this library, and renders results. Model execution,
scheduling, verification, persistence, and comparison remain library-owned.

Local retained state has one current schema. Resume requires an exact current
manifest; old run directories are not upgraded in place.
