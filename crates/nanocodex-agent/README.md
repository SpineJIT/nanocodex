# Nanocodex Agent

The owned lifecycle for one headless `OpenAI` coding agent.

`nanocodex-agent` composes the Tower-native Responses state machine from
`nanocodex-oai-api` with the runtime from `nanocodex-tools`. A normal consumer
builds one agent, receives a cheap cloneable [`Nanocodex`] handle and an
independent [`AgentEvents`] stream, then submits ordered prompts.

## Quick start

```rust,no_run
use nanocodex_agent::{Nanocodex, OpenAi};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let (agent, _events) = Nanocodex::builder(openai)
    .instructions(
        "You are a Rust coding agent. Preserve unrelated work and run relevant tests.",
    )
    .workspace(std::env::current_dir()?)
    .build()?;

let result = agent
    .prompt("Explain the cause of the failing parser test.")
    .await?
    .await?;
println!("{}", result.final_message());
agent.shutdown().await?;
# Ok(())
# }
```

The first `await` means the private driver accepted and ordered the prompt.
[`Turn`] is both a per-turn event stream and a future for [`TurnResult`].
Awaiting the turn waits only for its result; event consumption is independent.

The private driver is the sole owner of mutable conversation, transport, tool,
and process state. Cloning [`Nanocodex`] only clones its command capability;
[`Nanocodex::spawn`] creates a clean sibling and [`Nanocodex::fork`] creates an
independent branch from committed history.

## Typed events

[`AgentEvents`] is optional and independent from turn results. Its raw
JSONL-compatible envelope remains lossless, while
[`AgentEvent::data`](nanocodex_agent::events::AgentEvent::data) provides a
normalized domain view:

```rust,no_run
use futures_util::StreamExt;
use nanocodex_agent::{
    Nanocodex, OpenAi,
    events::{AgentEventData, AssistantEvent},
};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let (agent, mut events) = Nanocodex::builder(openai)
    .instructions("Answer concisely and preserve exact identifiers.")
    .build()?;
let turn = agent.prompt("Explain the identifier req_7f3.").await?;

while let Some(event) = events.next().await {
    if let AgentEventData::Assistant(AssistantEvent::Delta(delta)) = event.data()? {
        print!("{}", delta.text);
    }
    if event.kind.is_terminal() {
        break;
    }
}
let _result = turn.await?;
agent.shutdown().await?;
# Ok(())
# }
```

## Components

- [`events`](nanocodex_agent::events) contains the complete typed lifecycle
  event taxonomy.
- [`input`](nanocodex_agent::input) contains prompts and multimodal user input.
- [`session`](nanocodex_agent::session) contains durable session identities and
  snapshots.
- [`usage`](nanocodex_agent::usage) contains token accounting and USD estimates.
- [`rollout`](nanocodex_agent::rollout) records and restores Codex-compatible
  sessions.
- [`transport`](nanocodex_agent::transport) exposes advanced Responses and
  Tower configuration.
- [`tools`](nanocodex_agent::tools) exposes the complete tool implementation
  surface.

OpenAI API-key and managed ChatGPT credentials belong to
[`nanocodex_oai_api::auth`], independently of this lifecycle crate.
