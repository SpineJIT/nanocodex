//! Tool building blocks for `OpenAI`-compatible agents.
//!
//! This crate is useful without the Nanocodex agent loop. It provides the
//! caller-defined [`Tool`] contract, [`tool`] macro, heterogeneous [`Tools`]
//! registry, Code Mode runtime, standard workspace tools, and native MCP
//! clients. The dependency-light contract types are defined by
//! `nanocodex-oai-api` and re-exported here so a tool implementation has one
//! import surface.
//!
//! # Define and select tools
//!
//! The definition is the single source of truth for a tool's registry name.
//! The macro derives its input and output schemas from the function:
//!
//! ```
//! use nanocodex_tools::{Tools, tool};
//!
//! #[tool(
//!     name = "deployment_region",
//!     description = "Return the production region for a named service."
//! )]
//! async fn deployment_region(service: String) -> Result<String, std::io::Error> {
//!     Ok(format!("{service}: us-west-2"))
//! }
//!
//! # fn build() -> Result<(), nanocodex_tools::ToolsBuildError> {
//! let tools = Tools::builder()
//!     .without_defaults()
//!     .tool(deployment_region)
//!     .build()?;
//! # Ok(())
//! # }
//! ```
//!
//! Implement [`Tool`] directly when execution needs [`ToolContext`], freeform
//! input, multimodal [`ToolOutput`], or a custom definition.
//!
//! # MCP is native and always available
//!
//! MCP is not a feature flag. Native consumers configure stdio or Streamable
//! HTTP servers and install the provider into the same registry:
//!
//! ```
//! use nanocodex_tools::{Mcp, McpServer, Tools};
//!
//! # fn build() -> Result<(), Box<dyn std::error::Error>> {
//! let mcp = Mcp::builder()
//!     .server(
//!         "company_docs",
//!         McpServer::stdio("company-docs-mcp").arg("--readonly"),
//!     )
//!     .build()?;
//!
//! let tools = Tools::builder().provider(mcp).build()?;
//! # Ok(())
//! # }
//! ```
//!
//! Handshakes and discovery start with the owning runtime. [`mcp::Mcp`] exposes
//! only `tool_search` directly and activates matching remote definitions for
//! Code Mode, keeping large catalogs out of the model's initial tool list.

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]
#![cfg_attr(target_family = "wasm", allow(clippy::module_name_repetitions))]

#[cfg(not(target_family = "wasm"))]
mod apply_patch;
#[cfg(all(not(target_family = "wasm"), feature = "code-mode"))]
mod code_mode;
#[cfg(not(target_family = "wasm"))]
mod image;
#[cfg(all(not(target_family = "wasm"), feature = "remote-tools"))]
mod image_generation;
#[cfg(not(target_family = "wasm"))]
pub mod mcp;
#[cfg(not(target_family = "wasm"))]
mod plan;
#[cfg(not(target_family = "wasm"))]
mod runtime;
#[cfg(not(target_family = "wasm"))]
mod shell;
#[cfg(not(target_family = "wasm"))]
mod standard;
#[cfg(not(target_family = "wasm"))]
mod view_image;
#[cfg(target_family = "wasm")]
mod wasm;
#[cfg(all(not(target_family = "wasm"), feature = "remote-tools"))]
mod web_search;

#[cfg(all(not(target_family = "wasm"), feature = "code-mode"))]
pub use code_mode::{CodeModeExecution, CodeModeObserver, CodeModeUpdate, NestedToolCall};
#[cfg(not(target_family = "wasm"))]
pub use image::{prepare_output_images, prepare_user_input};
#[cfg(not(target_family = "wasm"))]
pub use mcp::{
    Mcp, McpBuildError, McpBuilder, McpControlError, McpHandle, McpLogin, McpOAuthCredentials,
    McpOAuthStore, McpServer,
};
pub use nanocodex_oai_api::{
    DEFAULT_TOOL_OUTPUT_TOKENS, ImageDetail, ProcessTraceWire, Tool, ToolContext, ToolDefinition,
    ToolError, ToolExecution, ToolExecutionWire, ToolInput, ToolInputError, ToolOutput,
    ToolOutputBody, ToolOutputContent, ToolOutputWire, ToolResult,
};
#[cfg(not(target_family = "wasm"))]
pub use nanocodex_tools_macros::tool;
#[cfg(not(target_family = "wasm"))]
pub use plan::UpdatePlanTool;
#[cfg(not(target_family = "wasm"))]
pub use runtime::{
    DynamicToolProvider, ImageGenerationConfig, OwnedToolContext, ToolRuntime, ToolRuntimeControl,
    Tools, ToolsBuildError, ToolsBuilder, WebSearchConfig, schema_for,
};
#[cfg(not(target_family = "wasm"))]
pub use standard::StandardTool;
#[cfg(target_family = "wasm")]
pub use wasm::*;

#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
pub mod __private {
    pub use async_trait::async_trait;
    pub use schemars;
    pub use serde;

    pub use crate::schema_for;
}
