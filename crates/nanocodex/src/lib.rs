//! Facade for the Nanocodex frontier-agent building blocks.
//!
//! This crate contains no runtime implementation. It reexports the owned
//! lifecycle from `nanocodex-agent` and provides named component modules for
//! lower-level use. Depending on `nanocodex-agent` directly creates the same
//! agent; the facade is only the convenient, batteries-included import path.
//!
//! ```no_run
//! use nanocodex::{Nanocodex, OpenAi};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
//! let (agent, _events) = Nanocodex::builder(openai)
//! .instructions(
//!     "You are a Rust coding agent. Preserve unrelated work and run relevant tests.",
//! )
//! .workspace(std::env::current_dir()?)
//! .build()?;
//!
//! let result = agent
//!     .prompt("Explain the cause of the failing parser test.")
//!     .await?
//!     .await?;
//! println!("{}", result.final_message());
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

pub use nanocodex_agent::*;

/// Owned agent lifecycle, builders, turns, branching, and snapshots.
pub mod agent {
    pub use nanocodex_agent::*;
}

/// Tower-native `OpenAI` Responses client, state machine, and wire types.
pub mod oai {
    pub use nanocodex_oai_api::*;
}

/// Tool contracts, registry, Code Mode, built-ins, and MCP.
pub mod tools {
    pub use nanocodex_tools::*;
}

/// Attribute macros for defining typed application tools.
#[cfg(not(target_family = "wasm"))]
pub mod macros {
    pub use nanocodex_tools::tool;
}

/// Application-owned tracing and OpenTelemetry setup.
#[cfg(not(target_family = "wasm"))]
pub mod observability {
    pub use nanocodex_observability::*;
}

/// Common imports for the golden owned-agent path.
pub mod prelude {
    pub use nanocodex_agent::{
        AgentEventData, AgentEvents, AssistantEvent, ContextEvent, EstimatedUsdCost, ModelEvent,
        NanocodexError, OpenAi, OpenAiAuth, PricingSnapshot, Prompt, ReasoningEvent, ReasoningMode,
        RunEvent, SessionId, SessionSnapshot, Thinking, TokenRates, ToolEvent, TurnUsage,
        UsdAmount, UsdPerMillionTokens,
    };
    #[cfg(not(target_family = "wasm"))]
    pub use nanocodex_agent::{Nanocodex, NanocodexBuilder, Turn, TurnControl, TurnResult};
    #[cfg(target_family = "wasm")]
    pub use nanocodex_agent::{WasmNanocodex, WasmTurn};
    #[cfg(not(target_family = "wasm"))]
    pub use nanocodex_tools::{Tool, ToolOutput, Tools, tool};
}
