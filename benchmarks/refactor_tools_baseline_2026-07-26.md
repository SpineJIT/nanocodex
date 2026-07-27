# Refactor tools baseline — 2026-07-26

This is the retained performance and parity record for refactor stack 3. It
moves the tool contract into `nanocodex-oai-api`, merges MCP into
`nanocodex-tools`, and renames the macro implementation to
`nanocodex-tools-macros`.

## Environment

- Apple M1 Max, arm64
- macOS 26.3.1
- rustc/cargo 1.97.1
- optimized Cargo bench profile
- Criterion 0.7, 100 samples

Commands:

```text
cargo bench -p nanocodex-tools --bench mcp_tool_search -- --noplot
cargo bench -p nanocodex-tools --bench tool_process_output -- --noplot
```

## Baseline and SLA

The p50 and p95 columns are calculated from Criterion's per-iteration sample
times. The hard SLA keeps local harness work materially below model and network
latency. On the same runner, a p50 regression over 25% requires investigation
and a written explanation even when it remains below the hard bound.

| Path | Workload | p50 | p95 | Hard p50 SLA |
| --- | --- | ---: | ---: | ---: |
| warm MCP search | 1 discovered tool, top 8 | 2.61 µs | 5.68 µs | 10 µs |
| warm MCP search | 1,000 discovered tools, top 8 | 153.35 µs | 391.29 µs | 500 µs |
| warm MCP dispatch | retained stdio client, one JSON call | 1.65 ms | 7.02 ms | 10 ms |
| process/output | spawn shell, capture 64 KiB, bound to 1,024 tokens | 12.65 ms | 37.93 ms | 50 ms |

The benchmark fixture performs real RMCP initialize/discovery and stdio RPC.
The process benchmark uses the public `ToolRuntime` and `exec_command` path,
including process startup, bounded capture, token estimation, and result
encoding.

## Validation

The parity ledger below was checked through every supported consumer boundary:

```text
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo clippy -p nanocodex-oai-api -p nanocodex-tools \
  --target wasm32-unknown-unknown --no-default-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc \
  -p nanocodex-oai-api -p nanocodex-tools-macros \
  -p nanocodex-tools -p nanocodex --all-features --no-deps
./scripts/check-docs.sh
cargo deny check
typos
just test-wasm
OPENAI_API_KEY= just test-python
docker build --check -f harbor_adapter/nanocodex.Dockerfile .
docker build --target artifact -f harbor_adapter/nanocodex.Dockerfile .
```

All release crates were also packaged with the release workflow's local patch
configuration. Packaged-source documentation passed with warnings denied, and
the packaged `nanocodex-tools` archive contains the MCP fixture and both
benchmarks.

## Master parity ledger

The old owner is removed only after its replacement is exercised.

| Master capability | New owner | Evidence |
| --- | --- | --- |
| caller-defined `Tool`, typed input/output/context, process wire form | `nanocodex-oai-api` | OAI doctest plus workspace tests |
| `#[tool]` through facade | `nanocodex-tools-macros` via `nanocodex-tools` | facade integration test |
| `#[tool]` without facade | `nanocodex-tools` | direct integration test |
| standard shell, stdin, patch, plan, image, web, image generation | `nanocodex-tools` | focused runtime suites |
| retained Code Mode cells, nested calls, concurrency, cancellation | `nanocodex-tools` | Code Mode suite and parent-span test |
| MCP stdio and Streamable HTTP | `nanocodex-tools::mcp` | stdio integration and ignored live HTTP smoke |
| background discovery, BM25 `tool_search`, deferred activation | `nanocodex-tools::mcp` | search/call integration and benchmarks |
| OAuth load/save/refresh/login inputs | `nanocodex-tools::mcp` | OAuth refresh/persistence suite |
| hot reload without provider restart | `nanocodex-tools::mcp` | reload integration test |
| bounded concurrent startup and dispatch | `nanocodex-tools::mcp` | 8-server/256-call stress test |
| browser/Node WASM host adapter | `nanocodex-tools` | wasm32 warning-denied check |
| CLI MCP config, login/reload, and observability | existing facade/CLI consumer | workspace compile and CLI tests |
