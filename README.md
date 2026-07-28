<div align="center">

<h1>Nanocodex</h1>

<p><strong>Building blocks for frontier OpenAI agents.</strong></p>

[![CI](https://img.shields.io/github/actions/workflow/status/gakonst/nanocodex/ci.yml?branch=master)][ci]
[![Crates.io](https://img.shields.io/crates/v/nanocodex.svg)][crates]
[![Docs.rs](https://img.shields.io/docsrs/nanocodex)][docs]
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)][license]

**[Install](#install)** · **[Agent API](#minimal-api-example)** ·
**[Thesis](#thesis)** · **[Components](#components)** ·
**[Documentation](#documentation)**

[ci]: https://github.com/gakonst/nanocodex/actions/workflows/ci.yml
[crates]: https://crates.io/crates/nanocodex
[docs]: https://docs.rs/nanocodex
[license]: LICENSE-MIT

</div>

## Install

Install the Nanocodex CLI on macOS or Linux:

```sh
curl -fsSL https://nanocodex.paradigm.xyz | bash
```

Or add the Rust SDK to an application:

```sh
cargo add nanocodex
```

## Minimal API Example

```rust,ignore
use nanocodex::{Nanocodex, OpenAi};

let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let (agent, mut events) = Nanocodex::builder(openai)
    .instructions(
        "You are a Rust coding agent. Make focused changes, preserve unrelated work, \
         and run relevant tests before finishing.",
    )
    .workspace(std::env::current_dir()?)
    .build()?;

let event_task = tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        eprintln!("event {}: {:?}", event.seq, event.kind);
        if event.kind.is_terminal() {
            break;
        }
    }
});

// Alternative: keep the `Turn` returned by the first `await` and consume its
// `Stream<Item = AgentEvent>` implementation to render output as it arrives.
let result = agent
    .prompt("Find and fix the failing parser test.")
    .await?
    .await?;

event_task.await?;
println!("{}", result.final_message());
```

The first `await` accepts and orders the prompt. The second waits for its typed
`TurnResult`. Follow-on prompts automatically reuse the agent's retained
history, WebSocket, tools, shell sessions, and prompt-cache identity.
`agent.clone()` is a cheap handle to that same session; the independently
returned `AgentEvents` stream is the session-wide event firehose.

## Thesis

### Small, excellent building blocks

Agent infrastructure is easier to understand and reuse when each piece has a
sharp owner and a useful API of its own. An OpenAI client should work without
an agent loop. Tools should work without a CLI. The high-level agent should
compose those pieces rather than hide another implementation of them.

Nanocodex makes a small number of deliberate choices—Rust, Tower, typed
protocols, owned lifecycle state, and builder APIs—then keeps the boundaries
boring.

### The model and harness are co-designed

We do not try to outsmart behavior that frontier models and Codex already make
explicit. Context management, `AGENTS.md`, compaction, cache identity, tool
shapes, continuation, reconnect replay, cancellation, and process cleanup are
parts of the model-facing contract.

Nanocodex carries those invariants into a smaller, library-first API while
leaving application policy with the caller.

### Evidence over intuition

Representative `cargo bench` workloads, OpenTelemetry traces, differential
tests, and end-to-end evals keep the harness honest. The goal is simple: normal
agent turns should be model- and network-latency bound, with token usage and
estimated USD cost visible at the same typed boundary as the result.

## Components

```text
nanocodex                         Alloy-style facade and prelude
├── agent                         nanocodex-agent
│   ├── oai                       nanocodex-oai-api
│   └── tools                     nanocodex-tools
│       └── macros                nanocodex-tools-macros
├── oai                           nanocodex-oai-api
├── tools                         nanocodex-tools
└── observability                 nanocodex-observability (optional)
```

The facade provides the canonical common imports. Each lower crate is also
designed to be useful directly, without importing the higher orchestration
layer.

### `nanocodex`

The thin facade reexports the golden agent path at the crate root and keeps
detailed APIs under `nanocodex::agent`, `nanocodex::oai`, and
`nanocodex::tools`. Its prelude contains only the common types needed to build
an agent.

[Facade guide](crates/nanocodex/README.md) ·
[API documentation](https://docs.rs/nanocodex)

### `nanocodex-agent`

The batteries-included lifecycle: an owned private driver, a cheap cloneable
`Nanocodex` handle, typed `Turn` and `TurnResult` values, and an optional event
stream. It owns prompt ordering, the tool loop, `AGENTS.md` discovery,
compaction timing, cancellation, snapshots, and branching through `spawn`,
`fork`, and `fork_from`.

Callers never pass previous messages, response IDs, or tool results back into
the agent.

[Agent guide](crates/nanocodex-agent/README.md) ·
[API documentation](https://docs.rs/nanocodex-agent)

### `nanocodex-oai-api`

The complete OpenAI boundary: API-key and ChatGPT authentication, typed
Responses protocol values, a persistent WebSocket transport, client-owned
context, continuation and replay, automatic pricing, and a generic Tower
client.

Its standalone `OpenAi -> Session -> ResponseTurn -> Response` path provides a
managed conversation without taking on agent policy. Custom Tower layers and
services remain concrete and nameable—no boxing or global client is required.

[OpenAI API guide](crates/nanocodex-oai-api/README.md) ·
[API documentation](https://docs.rs/nanocodex-oai-api)

### `nanocodex-tools`

The model-facing tool runtime: the `Tool` contract, heterogeneous `Tools`
registry, standard workspace tools, shell and process lifecycle, Code Mode,
deferred `tool_search`, remote dispatch, and MCP. MCP is always available on
native targets.

Applications can implement `Tool` directly or use the reexported `#[tool]`
macro. The separate `nanocodex-tools-macros` package exists only for Rust's
procedural-macro boundary.

[Tools guide](crates/nanocodex-tools/README.md) ·
[API documentation](https://docs.rs/nanocodex-tools)

### `nanocodex-observability`

Application-owned tracing and OpenTelemetry setup for the data already flowing
through the agent. It provides structured lifecycle, model, tool, usage, cost,
cache, and latency telemetry without changing the core runtime path.

Enable the facade's `observability` feature or depend on the component
directly.

[Observability guide](crates/nanocodex-observability/README.md) ·
[API documentation](https://docs.rs/nanocodex-observability)

### CLI and language bindings

The CLI/TUI, Python package, Node/browser package, React bindings, and examples
are thin consumers of the same owned session API. They do not define a second
agent protocol.

[Examples](examples/README.md) · [JavaScript](js/README.md) ·
[Python](py/README.md) · [Web](web/README.md)

## Documentation

- [Facade API](https://docs.rs/nanocodex)
- [Migration from 0.2.x](docs/MIGRATING.md)
- [Examples](examples/README.md)
- [Benchmarks and retained measurements](benchmarks/)

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT License

at your option.
