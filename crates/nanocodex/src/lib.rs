#![doc = include_str!("../README.md")]
#![deny(missing_docs, rustdoc::broken_intra_doc_links)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub use nanocodex_agent::{
    AgentEvents, CostStatus, EstimatedUsdCost, NanocodexError, ServiceTier, TurnUsage, UsdAmount,
};
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub use nanocodex_agent::{Nanocodex, NanocodexBuilder, Turn, TurnControl, TurnResult};
#[cfg(target_family = "wasm")]
#[cfg_attr(docsrs, doc(cfg(target_family = "wasm")))]
pub use nanocodex_agent::{WasmNanocodex, WasmTurn};
pub use nanocodex_oai_api::{OpenAi, ReasoningMode, Thinking};
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub use nanocodex_tools::{Tool, Tools, tool};

/// Owned agent lifecycle, builders, turns, branching, and snapshots.
pub mod agent {
    #![doc = include_str!("../../nanocodex-agent/README.md")]

    #[doc(inline)]
    pub use nanocodex_agent::*;
    pub use nanocodex_agent::{events, input, session, tools, usage};
    #[cfg(not(target_family = "wasm"))]
    #[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
    pub use nanocodex_agent::{rollout, transport};
}

pub mod oai {
    #![doc = include_str!("../../nanocodex-oai-api/README.md")]

    #[doc(inline)]
    pub use nanocodex_oai_api::*;
}

pub mod tools {
    #![doc = include_str!("../../nanocodex-tools/README.md")]

    #[doc(inline)]
    pub use nanocodex_tools::*;
}

/// Application-owned tracing and OpenTelemetry setup.
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub mod observability {
    #![doc = include_str!("../../nanocodex-observability/README.md")]

    #[doc(inline)]
    pub use nanocodex_observability::*;
}

/// Common imports for the golden owned-agent path.
pub mod prelude {
    #[cfg(not(target_family = "wasm"))]
    #[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
    pub use crate::{Nanocodex, NanocodexBuilder, OpenAi, Tool, Tools, tool};
    #[cfg(target_family = "wasm")]
    #[cfg_attr(docsrs, doc(cfg(target_family = "wasm")))]
    pub use crate::{OpenAi, WasmNanocodex, WasmTurn};
}

#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
pub mod __private {
    pub use nanocodex_tools::__private::*;
}
