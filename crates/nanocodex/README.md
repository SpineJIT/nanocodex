# Nanocodex

The batteries-included façade for the Nanocodex frontier-agent building blocks.

This crate contains no second runtime implementation. It re-exports the owned
agent lifecycle and gives the lower-level crates stable, named module paths.
Depending on `nanocodex-agent` directly creates the same agent.

## Quick start

Build one owned agent, keep its cheap cloneable handle, and await typed turn
results. The independent event stream is optional:

```rust,no_run
use nanocodex::{Nanocodex, OpenAi};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let (agent, _events) = Nanocodex::builder(openai)
    .instructions(
        "You are a Rust coding agent. Preserve unrelated work and run relevant tests.",
    )
    .workspace(std::env::current_dir()?)
    .build()?;

let turn = agent
    .prompt("Explain the cause of the failing parser test.")
    .await?;
let result = turn.await?;

println!("{}", result.final_message());
# Ok(())
# }
```

Awaiting `prompt` means the private driver accepted and ordered the turn.
Awaiting the returned [`Turn`] drains that turn and yields its complete
[`TurnResult`]. Follow-on prompts reuse the same retained context and transport
without asking the caller to manage response IDs or history.

## Usage and USD estimates

Every completed turn reports aggregate provider usage. Cost remains explicit:
Nanocodex applies OpenAI's published `gpt-5.6-sol` standard or priority rates
automatically, while omitted provider usage remains distinguishable from zero.

```rust,no_run
use nanocodex::{Nanocodex, OpenAi};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let (agent, _events) = Nanocodex::builder(openai)
    .instructions("Answer concisely and preserve exact identifiers.")
    .build()?;

let result = agent.prompt("Explain the identifier req_7f3.").await?.await?;
if let Some(cost) = result.usage().estimated_cost() {
    println!("estimated {}", cost.amount());
} else {
    println!("cost unavailable: {}", result.usage().cost_status().as_str());
}
# Ok(())
# }
```

## Progressive disclosure

The root exports only the golden-path types. Reach for a named module when an
embedding needs more control:

- [`agent`] — lifecycle policy, events, input, sessions, usage, and rollout
- [`oai`] — managed Responses sessions and the concrete Tower boundary
- [`tools`] — tool contracts, built-ins, Code Mode, and MCP
- [`observability`] — native tracing and OTLP setup
- [`prelude`] — common imports for the owned-agent path

Each component module carries the same guide as its independently usable crate.
