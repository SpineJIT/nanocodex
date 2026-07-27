# Observability and cost baseline — 2026-07-26

This baseline covers the work added by the normalized agent-event projection
and exact USD estimation. It is not a provider-latency claim.

## Environment and fixtures

- Apple M1 Max (`arm64`)
- macOS 26.3.1
- `rustc 1.97.1 (8bab26f4f 2026-07-14)`
- optimized `bench` profile
- one retained Nanoeval agent JSONL: 358 events, 386,437 bytes, including 251
  inbound OpenAI events, model calls, tools, and one terminal result

The unredacted retained trace remains outside Git. The benchmark validates that
every retained event satisfies its typed projection before timing it.

## Commands

```text
cargo bench -p nanocodex-oai-api --bench tower_responses -- pricing_estimation --quick

NANOCODEX_BENCH_EVENTS=/path/to/retained/agent/events.jsonl \
  cargo bench -p nanocodex-oai-api --bench tower_responses -- \
  retained_agent_event_trace --quick
```

## Baseline

| Workload | Median estimate | Normalized cost |
| --- | ---: | ---: |
| Estimate one aggregate turn from four token classes | 25.980 ns | one result |
| Decode retained JSONL into raw-payload envelopes | 318.98 µs | 891 ns/event |
| Project all retained envelopes into typed domain events | 233.23 µs | 652 ns/event |
| Re-encode retained raw-payload envelopes | 48.494 µs | 135 ns/event |

The typed projection is lazy. Event emission, session/turn mirroring, and JSONL
serialization retain the existing `Arc<RawValue>` path and pay no projection
cost. The TUI requests typed data only for event kinds it renders; lower-level
OpenAI and transport events remain raw unless a consumer explicitly projects
them.

## Regression contracts

- USD estimation uses fixed-point arithmetic, performs no I/O or global lookup,
  and remains below 100 ns p50 on this runner.
- Projecting the complete retained trace remains below 1.5 µs/event p50 on this
  runner. Changes above that budget require a retained-trace explanation.
- Raw JSONL decode followed by encode is byte-identical for the retained trace.
- A dropped event receiver continues to skip payload serialization.
- Session and turn receivers share the retained raw payload. Delivery remains
  lossless and nonblocking for the agent, so unread retained memory is
  intentionally proportional to unread event data rather than hidden behind a
  bounded queue that drops records or stalls result-only consumers.
- Pricing absence and provider-omitted usage have distinct typed statuses;
  neither can produce a numeric zero estimate.
- The same `EstimatedUsdCost` value is projected through the OAI response,
  agent result, terminal event, tracing, CLI/TUI, Python, and WASM paths.

Criterion output under `target/criterion` and the unredacted retained trace are
generated evidence and are intentionally not committed.
