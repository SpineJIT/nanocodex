# PR #50 performance milestone

This is the reproducible performance gate for the stable-API and
observability refactor on `refactor/05-observability`. It covers every changed
public hot path named in the milestone: request construction, authoritative
history replay and checkpoints, context accounting and compaction, event
delivery and cost calculation, caller-tool and Code Mode dispatch, MCP
discovery/search/dispatch, and retained-trace TUI state and rendering.

Run the complete gate with:

```console
just bench-pr50
```

The recipe runs named Criterion workloads and then
`scripts/check-benchmark-thresholds.sh`. The checked-in source of truth is
`benchmarks/pr50_thresholds.tsv`: 39 full-sample reference medians and absolute
maximum medians. Missing estimates fail closed. Absolute limits are deliberate:
Criterion's comparison baseline is local mutable state, while the checked-in
contract must be reviewable and stable.

## Measurement host

- Linux 6.8.0-136-generic, x86-64
- AMD EPYC 4585PX, 16 cores / 32 logical CPUs, 128 MiB L3
- rustc 1.97.0 (`2d8144b7880597b6e6d3dfd63a9a9efae3f533d3`)
- Cargo 1.97.0
- Source parent: `787b20cd9b323648cf52ec78f3619deab8d079d5`

The 2026-07-28 milestone run passed all 39 latency gates. Representative full
medians were 32.67 µs for a 128 KiB request, 200.75 µs for mirrored delivery of
1,024 contractual events, 1.70 ms from accepted prompt to typed result, 1.28 ms
for context accounting over 10,000 messages, 647.28 µs for representative
large-output compaction, 307.95 µs for warm Code Mode, 53.71 ms for cold
discovery of 1,000 MCP tools, and 95.87 µs for warm search over those tools.

The retained Codex TUI tail rendered in 53.56 µs at 80×24, 110.26 µs at
120×40, and 237.30 µs at 200×60. Resizing between the extremes took 241.54 µs.
The adversarial first frame for a one-megabyte single line took 23.99 ms. These
remain far below a model/network turn, so the harness is model-latency bound;
no claim is made about unmeasured paths.

Latency is not the only gate. The benchmark workloads also assert that:

- draining 128 queued viewport rows renders no more than 128 animation frames;
- a catch-up frame changes at most 2,400 cells and writes at most 4,096 bytes;
- a footer-only fast-mode toggle changes at most 48 cells and writes at most
  256 bytes.

The retained trace corpus used to derive the committed deterministic fixtures
remains outside Git, as required by the repository performance policy.

## Live model-latency boundary

Five alternating, sequential runs of the identical 10-turn plus three-fork
workload completed successfully for both Nanocodex
`b7b7df6c1e09d7e391181a5550c24bd73f3bda67` and local Codex
`3418498f01422f5f650ea645d4bd19e05c3a9616` using shared ChatGPT
authentication. Workload, generated-prompt, and `AGENTS.md` digests matched in
all ten runs.

Across Nanocodex's 70 measured turns, total wall latency was 130,665.66 ms and
reported model duration was 127,893.84 ms: model work accounted for 97.879% of
the measured turn time. Median local turn overhead was 0.267 ms. Constructing
three historical forks took 12.2 ms at p50, while warm model turns took
1,371.4 ms at p50. The corresponding stock-Codex medians were 155.3 ms and
1,555.3 ms. This paired result, rather than the local microbenchmarks alone,
establishes that the representative harness remains model-latency bound.

The complete trial records, usage/cache fields, outputs, and p50/p95
distributions are retained in
`paired-parity-b7b7df6-vs-3418498f-chatgpt.json` outside Git.
