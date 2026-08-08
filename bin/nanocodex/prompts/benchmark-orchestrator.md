Drive the host aggressively, but measure useful live work instead of treating
accepted ledger leases as capacity.

Time to first run is part of the contract. After the first status read and one
host-pressure snapshot, launch an initial set of independent runs no larger than
the host's logical CPU count and small enough to satisfy the outstanding-run
memory reservation below. Do not blindly launch one run per CPU. Spread runs
across prepared tasks and treatments. Do not design a scheduler first, generate
a scheduler script, create a background shell controller or temporary
orchestration file, or delegate scheduling to a long-lived child loop. Keep
scheduling in this agent through direct parallel tool calls and retain every
returned process session.

Maintain three separate measurements:

- Ledger `running` is the number of unexpired leases. It is progress metadata,
  not the number of useful workers and never a reason to launch more work.
- Live runs are retained, still-running `eval run` process sessions corroborated
  with the host process table. This is the authoritative fleet size.
- Materialized VMs are live VM processes. A newly launched run that has not yet
  materialized a VM can still consume a full VM allocation shortly, so reserve
  4 GiB for each such outstanding live run when predicting memory pressure.

Poll retained sessions and host processes before every refill. If ledger
`running` exceeds live runs by more than one ramp batch, assume dead processes
left active leases. Launch nothing until those leases expire or the discrepancy
otherwise resolves. Never replace a process merely because the ledger still
calls its abandoned lease running.

There is no fixed steady-state slot ceiling, but upward probes must be measured.
After the initial launch, add at most four runs at a time. Do not add another
batch for at least 60 seconds and until every run in the previous batch has
either materialized, reached a stable remote-model wait, or exited. Before
launching, subtract the outstanding-run reservation from `MemAvailable`. Keep
the predicted reserve above the greater of ten percent of physical RAM or
6 GiB. Stop refilling below that boundary and resume only after completed runs
return enough memory.

Use `MemAvailable`, active swap-in, memory PSI, VM allocation failures, retained
session exits, and kernel OOM events together. Inspect OOM history with the
kernel journal and `/proc/vmstat` for at least the lifetime of the current
service process and never less than the last 30 minutes. Any OOM kill means the
high-water mark was unsafe: stop all upward probes, let existing work drain, and
remain at least eight live runs below the fleet size that caused it for ten
minutes before considering another four-run probe. Sustained swap-in, non-zero
full memory PSI, or repeated VM allocation failure also pauses refills. Do not
kill healthy accepted work merely to hit a target.

Replace a completed or cleanly retried run promptly when the pressure rules
allow it. Model failures, verifier failures, and one broken treatment do not
justify idling unrelated healthy slots. If completed coordinates do not advance
for ten minutes while sessions churn, stop ramping and inspect representative
session outputs before launching anything else.
