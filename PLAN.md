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
   - Terminal-Bench 2.1 milestone evaluation is delegated to the user's
     separate thread. This thread does not bootstrap Harbor, alter eval inputs,
     or wait on that result.
   - Fix every real PR #50 CI failure and leave required checks green with no
     known merge blocker.

## Current execution order

1. [x] Complete the [Codex parity ledger](docs/CODEX_PARITY.md) from the pinned
   checkpoint through
   `openai/codex@be2e4afcd7392339d6adbaf0d31b26316bcaa2ab`.
2. [x] Finish the behavior-preserving rollout, model/run, tool/runtime, and
   driver module decompositions.
3. [x] Verify the documented parity contracts and fix confirmed mismatches
   test-first.
4. [x] Establish 39 benchmark thresholds, retained-trace TUI gates, and the
   full-fidelity observability path.
5. [x] Run the in-scope consumer, differential, documentation, and smoke gates.
   Terminal-Bench milestone evaluation remains delegated to the user's
   separate thread.
6. [x] Verify remote PR #50 head `c55293c` as `MERGEABLE`/`CLEAN` with all
   required checks green before this documentation audit.
7. [x] Correct stale public guides, add a `0.2.x` migration map, and rerun the
   focused documentation checks.
8. [ ] Classify the ten currently unreviewed local Codex commits from
   `be2e4afc` through `bb1af235` before advancing the parity checkpoint.
9. [x] Expose nameable generic Tower service-factory types and the standalone
   session's protocol-level tool definitions and paired outputs. Keep
   `nanocodex-tools::Tools` composition in the batteries-included agent rather
   than adding `Session::tools`.
10. [ ] Rerun required PR checks after the closeout changes and confirm the new
    remote head is mergeable.
11. [x] Add the library-first GPT Realtime voice slice: typed 24 kHz PCM
    input/output, API-key WebSocket and Codex-compatible ChatGPT WebRTC
    transports, plus an experimental `nanocodex-voice` default-device and
    background-agent lifecycle consumed by the thin Ratatui `/voice` adapter.
    ChatGPT voice uses the coding session's
    subscription credential and frameless sideband; when no host attestation
    exists, it sends Codex's accepted unavailable-token envelope. The TUI
    exposes Codex's current voice catalog through `/voice list` and named
    starts, with Codex's current `cove` default and Frameless model.

## Current non-goals

- No provider abstraction, generic app server, compatibility layer, approval
  subsystem, or alternate agent runtime.
- No browser audio-device ownership or generic realtime/app-server protocol in
  the core library.
- No new `.service(...)` transport design without a concrete consumer.
- No cosmetic CLI/TUI lifecycle rewrite when existing behavior is accepted.
- No further VM, browser, managed-agent, proxy, or experimental-crate work.
- No benchmark, task, or verifier modification made solely to improve an eval
  score.
