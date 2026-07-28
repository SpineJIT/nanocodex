# Nanocodex plan

## Objective

Build high-quality reusable Rust building blocks for frontier OpenAI agents.
Nanocodex makes a small number of deliberate choices about libraries, public
APIs, performance, and observability while following the supported Codex
harness behavior exactly. It does not reimplement policy already owned by the
model or harness.

Every stable crate must be useful independently, documented from its own
README, tested through its public paths, benchmarked at the boundaries it can
affect, and observable without adopting the Nanocodex CLI.

## PR #50 delivery boundary

PR #50 is the only active delivery target. It must preserve behavior available
on `master` unless a removal is explicit and covered by a regression or
migration, and it must be independently mergeable.

1. **Re-establish Codex parity**
   - Treat `openai/codex@35eaf3ffb0bf2001486c68c47a3d946b34d16634`
     as the last authoritative reviewed checkpoint.
   - Inspect and classify every later upstream commit before advancing that
     checkpoint.
   - Differentially verify prompt-cache identity and stable prefixes;
     `AGENTS.md` and environment injection; typed history and
     `previous_response_id`; reconnect/full replay; automatic/manual
     compaction; steering/cancellation; completed-only commits; retries and
     fallback; tool ordering, errors, panics, and process cleanup; and shared
     ChatGPT authentication.
   - Fix demonstrated mismatches test-first. Record intentional differences
     explicitly; do not silently call them parity.

2. **Stabilize crate ownership and public paths**
   - `nanocodex-oai-api` owns the complete OpenAI boundary and honest Tower
     seams.
   - `nanocodex-tools` owns tool implementations, Code Mode, MCP, and deferred
     search.
   - `nanocodex-agent` owns the private driver, lifecycle, state, branching,
     snapshots, and rollouts.
   - `nanocodex` remains a thin Alloy-style facade.
   - Keep mutable run configuration, events plumbing, attempt factories,
     response/turn IDs, queues, sockets, and replay bookkeeping private.
   - Remove accidental exports, compatibility leftovers, duplicate bindings,
     empty directories, unused dependencies/features, and unnecessary cfgs.

3. **Make the stable APIs legible**
   - Give each stable crate a focused README included into crate docs.
   - Put the normal consumer path first and advanced Tower/protocol surfaces
     behind progressive disclosure.
   - Compile complete public examples through canonical paths.
   - Keep `OpenAiBuilder::{layer,service}` as the deliberate transport seam.

4. **Lock in performance and observability**
   - Define representative benchmarks and explicit thresholds for request
     construction, history replay/checkpointing, context accounting and
     compaction, event delivery, tool dispatch, Code Mode, MCP discovery/search,
     and changed TUI state/render work.
   - Follow init4-style bounded spans and explicit parent propagation while
     keeping contractual events independent from tracing.
   - Preserve full-fidelity ordered prompts, model traffic, reasoning and
     encrypted reasoning, tool activity, steering, cancellation, token/cache
     data, latency, and automatic `gpt-5.6-sol` USD cost.

5. **Prove the complete PR path**
   - Validate crate boundaries, formatting, warnings-denied Clippy, workspace
     and all-target tests, rustdoc/doctests/examples, WASM, Node/browser, PyO3,
     CLI/Ratatui, and a live native smoke.
   - Run the stock-Codex differential suite.
   - Run the complete configured Terminal-Bench 2.1 milestone eval without
     changing tasks or verifiers, then inspect exact JSONL, trajectories,
     verifier output, timing, and cost artifacts.
   - Fix every real PR #50 CI failure and leave required checks green with no
     known merge blocker.

## Current execution order

1. [x] Complete the [Codex parity ledger](docs/CODEX_PARITY.md) from the pinned
   checkpoint through local `openai/codex@3418498f01422f5f650ea645d4bd19e05c3a9616`.
2. [x] Finish the behavior-preserving rollout, model/run, tool/runtime, and
   driver module decompositions.
3. [x] Audit stable public paths, crate docs, examples, dependencies, features,
   cfgs, and crate boundaries.
4. [x] Verify each parity contract and fix confirmed mismatches test-first.
5. [x] Finish benchmark thresholds and full-fidelity observability verification.
6. [ ] Run all consumer, differential, smoke, and milestone eval gates.
7. [ ] Commit and push coherent PR #50 slices, remediate CI, and verify the
   pull request is mergeable with green required checks.

## Current non-goals

- No provider abstraction, generic app server, compatibility layer, approval
  subsystem, or alternate agent runtime.
- No audio implementation work.
- No new `.service(...)` transport design without a concrete consumer.
- No cosmetic CLI/TUI lifecycle rewrite when existing behavior is accepted.
- No VM, browser, managed-agent, proxy, or experimental-crate work.
- No benchmark, task, or verifier modification made solely to improve an eval
  score.
