# Agent extraction baseline — 2026-07-26

This baseline stabilizes the costs changed by the `nanocodex-agent` extraction
and per-turn stream. It is not a provider-latency claim.

## Environment

- Apple M1 Max (`arm64`)
- macOS 26.3.1
- `rustc 1.97.1 (8bab26f4f 2026-07-14)`
- Criterion quick mode, optimized `bench` profile

## Commands

```text
cargo bench -p nanocodex-agent --bench agent_lifecycle -- --quick
cargo bench -p nanocodex-oai-api --bench tower_responses -- timed_agent_event_delivery --quick
```

The lifecycle workload uses a real private driver, context manager, tool
runtime, event path, and typed result with an immediate in-memory Tower
service. The event workload serializes and delivers 1,024 contractual events.

## Baseline

| Workload | Median estimate |
| --- | ---: |
| Clone a `Nanocodex` handle | 36.609 ns |
| Fresh agent + accepted first turn + typed result | 373.36 µs |
| Emit/drain 1,024 session events | 148.62 µs |
| Emit/drain the same 1,024 events through session + turn streams | 187.19 µs |

The per-turn mirror adds about 38.6 µs per 1,024 events, or 37.7 ns per event,
without duplicating the retained raw JSON payload. At this scale the complete
fresh-agent path is still below 0.4 ms, so normal work remains provider/model
latency bound.

Re-running the lifecycle benchmark after moving authoritative conversation
state into the OAI-owned engine reported no statistically detectable
regression (`p = 0.73` for the complete first-turn path).

## Regression contract

- Handle cloning remains constant-time and must not allocate, open a runtime
  resource, or clone conversation/tool state.
- A turn mirror shares the immutable payload and performs no payload-sized
  clone. Its work remains linear only in emitted event count.
- Prompt acceptance remains independent of retained history length; the driver
  owns FIFO ordering and bounded backpressure.
- Healthy follow-on requests remain delta-sized and forks retain shared
  committed history.
- Numeric gates are introduced from repeated non-quick baselines on the same
  runner. Until then, a change above 2× these medians requires investigation
  and retained-trace validation rather than automatic acceptance.

Criterion JSON/HTML output under `target/criterion` is generated evidence and
is intentionally not committed.

## Master parity ledger

`master` remains the behavioral baseline. The extraction does not delete an
agent capability; each existing path is exercised through its new owner:

| Master capability | Refactored owner and executable evidence |
| --- | --- |
| API-key and managed ChatGPT OAuth authentication | `nanocodex-agent` auth tests plus standard, subscription-WebSocket, and subscription-HTTPS model tests |
| Persistent WebSocket, HTTPS/SSE, stored and ephemeral history | `nanocodex-oai-api` transport tests plus agent incremental, full-replay, reconnect, and checkpoint-miss tests |
| Caller Tower layers and fresh service factories | agent builder, child, cancellation, and configured-attempt-limit tests |
| Warmup and shared-prefix singleflight | agent warmup failure/fallback and cloned-builder singleflight tests |
| Follow-on prompts and per-turn reasoning policy | follow-on and accepted-policy tests |
| Steering and bounded prompt ordering | FIFO steering, tool-boundary steering, and queued-prompt tests |
| Queued and active cancellation with repaired history | driver cancellation, active-tool pairing, and resumed-abort-boundary tests |
| Clone, clean spawn, latest fork, historical `fork_from` | builder/driver tests plus active-prompt, tool/steer, historical, recursive-agent-tool, and checkpoint-eviction tests |
| Compaction and context regeneration | pre-turn and in-turn compaction tests plus rollout replacement-boundary tests |
| Serializable resume and Codex-compatible rollouts | snapshot/rollout shared-history, repair, append, and ephemeral-resume tests |
| Code Mode, built-in/custom tools, MCP, web and image paths | `nanocodex-tools` stack-3 parity suite plus agent Code Mode, image, notification, and unsupported-call tests |
| Session and per-turn typed event streams | mirrored-stream ordering test and complete-turn stream/result parity test |
| Rust facade, CLI/TUI, PyO3, Node/browser WASM, examples, and Harbor adapter | warning-denied workspace all-target/all-feature check, compiled examples, binding tests, and the final Docker/Harbor smoke gate |
| Full-content tracing and attached child parentage | agent tracing integration test and existing transport/tool tracing suites |

The OAI standalone `Session` and the owned agent now share one managed state
engine for typed history, token estimation, delta/continuation state,
compaction installation, replay reset, and history revisions. Agent-only
workspace, tool, warmup, steering, cancellation, and fork policy remains in
`nanocodex-agent`.
