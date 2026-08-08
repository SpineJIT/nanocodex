Drive the host aggressively, but measure useful live work instead of treating
accepted ledger leases as capacity. The launch gates below are hard
preconditions, not targets or suggestions. They override time-to-first-run and
the desire to saturate the host.

Immediately before every tool call that starts one or more `eval run` commands:

1. Read the ledger, poll every retained process session, and corroborate the
   live run and materialized VM counts with the host process table.
2. If ledger `running` is greater than live runs plus four, launch exactly zero
   runs. Dead processes have left leases behind. Poll until the discrepancy is
   at most four; do not compensate for it.
3. Let `outstanding = live runs - materialized VMs`, clamped at zero, and let
   `host_reserve = max(10% of MemTotal, 6 GiB)`. Compute
   `memory_cap = floor((MemAvailable - host_reserve) / 4 GiB) - outstanding`.
   Clamp it at zero. The launch count must never exceed `memory_cap`.
4. Inspect recent kernel OOM evidence with both
   `journalctl -k --since '30 minutes ago' --no-pager` and the `oom_kill` row in
   `/proc/vmstat`. Do not use `dmesg --since`; it is not a valid substitute and
   previously caused this host's OOM kills to be missed.

State the measured ledger, live run, VM, memory, and resulting launch-cap
numbers before launching. A launch that violates any gate is forbidden.

Time to first run is part of the contract. After the first status read and one
host-pressure snapshot, launch an initial set of independent runs no larger than
the host's logical CPU count, `memory_cap`, and the pending coordinate count. Do
not blindly launch one run per CPU. Spread runs across prepared tasks and
treatments. Do not design a scheduler first, generate a scheduler script, create
a background shell controller or temporary orchestration file, or delegate
scheduling to a long-lived child loop. Keep scheduling in this agent through
direct parallel tool calls and retain every returned process session.

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
left active leases and apply launch gate 2. Never replace a process merely
because the ledger still calls its abandoned lease running.

There is no fixed steady-state slot ceiling, but upward probes must be measured.
After the initial launch, add at most four runs at a time. Do not add another
batch for at least 60 seconds and until every run in the previous batch has
either materialized, reached a stable remote-model wait, or exited. Before
launching, recompute and obey `memory_cap`; a refill count is the smaller of four
and that cap. Stop refilling when the cap is zero and resume only after completed
runs return enough memory.

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
