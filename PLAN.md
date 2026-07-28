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

## Delivery stack

The refactor ships as three stacked, independently mergeable pull requests.
Each PR must preserve all behavior available on `master` unless its removal is
explicitly agreed and covered by a regression or migration.

### PR 1 — Stable API refactor

This is the current work on PR #50.

1. **Re-establish Codex parity**
   - Treat `openai/codex@35eaf3ffb0bf2001486c68c47a3d946b34d16634`
     as the last authoritative reviewed checkpoint.
   - Inspect and classify every later upstream commit before advancing that
     checkpoint. The current pending range through `8431dc590a` contains 37
     commits.
   - Compare exact agent lifecycle, Responses transport, and tool behavior:
     prompt-cache identity and stable prefixes; `AGENTS.md` and environment
     injection; typed history and `previous_response_id`; reconnect and full
     replay; automatic and manual compaction; steering and cancellation;
     completed-only commits; tool definitions, results, errors, and process
     cleanup.
   - Fix demonstrated mismatches test-first. Record intentional differences
     explicitly; do not silently call them parity.

2. **Stabilize crate ownership**
   - `nanocodex-oai-api`: typed OpenAI Responses protocol, authentication,
     context state, WebSocket/HTTPS transports, Tower service/client, retries,
     telemetry, and the minimal shared tool contracts needed at the API
     boundary.
   - `nanocodex-tools`: the public `Tool` implementation layer, built-ins, Code
     Mode, MCP including `tool_search`, and the colocated macros package.
   - `nanocodex-agent`: the owned agent loop, context and lifecycle policy,
     cloneable handles, turns, steering, cancellation, checkpoints, and forks.
   - `nanocodex`: a thin Alloy-style facade with deliberate reexports and a
     prelude.
   - `bin/nanocodex`: a consumer of those libraries, not a second agent
     implementation.
   - Remove compatibility crates, duplicate bindings/runtime code, and empty
     folders rather than carrying adapters that only move data.

3. **Make the stable APIs legible**
   - Give each crate a focused README included into its crate docs.
   - Make the normal path visible first in `cargo doc`; advanced Tower,
     protocol, and embedding surfaces follow through progressive disclosure.
   - Compile every public example. Examples use real instructions and complete
     values rather than unexplained placeholders.
   - Keep `.service(...)` and accepted CLI/TUI lifecycle behavior unchanged
     unless a concrete parity or consumer failure requires a change.

4. **Lock in performance and observability**
   - Define representative Cargo benchmarks and regression thresholds for
     public hot paths: request construction, history replay/checkpointing,
     context accounting and compaction, event delivery, tool dispatch, Code
     Mode, MCP discovery/search, and TUI state/render work where changed.
   - Keep harness overhead small enough that normal execution remains
     model-latency bound. Parallelize independent startup and dispatch work
     where traces and benchmarks justify it.
   - Follow init4-style bounded spans and explicit parent propagation.
     Contractual events remain separate from tracing.
   - Preserve full-fidelity ordered prompts, responses, reasoning, tool
     activity, steering, cancellation, token usage, cache behavior, latency,
     and automatic `gpt-5.6-sol` USD cost.

5. **Prove the vertical path**
   - Preserve the CLI, Ratatui, PyO3, and Node/browser WASM consumers as thin
     adapters over the same owned session API.
   - Run formatting, warnings-denied Clippy, workspace tests, all-target
     checks, rustdoc/doctests, public examples, WASM checks, and `just run`.
   - Run the stock-Codex differential suite for request, context, tool, retry,
     reconnect, compaction, and cancellation behavior.
   - Use focused Harbor tasks while iterating. Run the complete configured eval
     only as the PR milestone gate, inspecting JSONL, trajectory, verifier, and
     timing artifacts before making a claim.

PR 1 is complete only when the public library path is feature-equivalent to
`master`, the verified Codex checkpoint is truthful, and the branch is ready
to merge without relying on the later PRs.

### PR 2 — `nanocodex-eval` and required VM machinery

1. Consolidate the temporary `nanoeval` work into this workspace as
   `nanocodex-eval`.
2. Expose evaluations through `nanocodex eval <...>` with Harbor-compatible
   task, verifier, artifact, JSONL, ATIF, token, latency, and USD accounting.
3. Add the minimal VM layer required by evaluations, including
   Dockerfile-derived pre-snapshotted disks and reusable pre-baked images.
4. Support full Terminal-Bench 2.1 and FrontierBench runs, including Daytona
   execution where configured. Do not weaken tasks or verifiers.
5. Produce on-demand PR build artifacts so a run can select a Nanocodex binary
   built from a pull request without enabling expensive evals on every change.
6. Separate cold image/bootstrap time from warm agent work and retain exact
   run artifacts for comparisons.

PR 2 is complete when `nanocodex eval ...` can run the full configured suite
against a selected local or PR build using the consolidated Rust/library path.

### PR 3 — Experimental managed-agent components

1. Add experimental browser-on-VM, Centaur durability/managed-agent work, and
   related proxy components under `crates/experimental/` where they are
   reusable libraries.
2. Keep executables and Tempo-specific integration under `bin/`.
3. Add the egress-VM boundary that encapsulates MPP payments and secrets egress
   without adding Tempo dependencies to stable Nanocodex crates.
4. Reuse the VM and eval foundations from PR 2; do not duplicate their runtime
   or artifact model.
5. Require a concrete consumer, focused tests, tracing, and benchmarks before
   promoting any experimental component into the stable crate graph.

## Current execution order

1. [x] Complete the [37-commit Codex parity ledger](docs/CODEX_PARITY.md) from
   `35eaf3` through `8431dc5`.
2. [ ] Turn each confirmed lifecycle, Responses, or tools mismatch into a
   deterministic failing regression, then fix it.
3. [ ] Review and consolidate the current PR #50 worktree without losing
   unrelated user work or `master` functionality.
4. [ ] Finish public-path docs, benchmarks, tracing, and consumer validation
   needed by the changed surfaces.
5. [ ] Run the PR 1 validation and differential gates.
6. [ ] Commit and push the reviewed PR #50 stack only when explicitly asked.
7. [ ] Begin PR 2 from the mergeable PR 1 boundary.

## Current non-goals

- No provider abstraction, generic app server, compatibility layer, approval
  subsystem, or alternate agent runtime.
- No audio implementation work.
- No new `.service(...)` transport design without a concrete consumer.
- No cosmetic CLI/TUI lifecycle rewrite when existing behavior is accepted.
- No browser, Centaur, proxy, or experimental VM surface in PR 1.
- No benchmark, task, or verifier modification made solely to improve an eval
  score.
