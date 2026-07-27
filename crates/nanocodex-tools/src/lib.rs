#![doc = include_str!("../README.md")]
#![deny(missing_docs, rustdoc::broken_intra_doc_links)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(target_family = "wasm", allow(clippy::module_name_repetitions))]

#[cfg(not(target_family = "wasm"))]
mod apply_patch;
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub mod code_mode;
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub mod image;
#[cfg(not(target_family = "wasm"))]
mod image_generation;
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub mod mcp;
#[cfg(not(target_family = "wasm"))]
mod plan;
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub mod runtime;
#[cfg(not(target_family = "wasm"))]
mod shell;
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub mod standard;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_support;
#[cfg(not(target_family = "wasm"))]
mod view_image;
#[cfg(target_family = "wasm")]
mod wasm;
#[cfg(not(target_family = "wasm"))]
mod web_search;

/// Model-visible tool definitions, inputs, outputs, and execution contracts.
pub mod contract {
    #[cfg(not(target_family = "wasm"))]
    #[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
    pub use async_trait::async_trait;
    pub use nanocodex_oai_api::tools::{
        DEFAULT_TOOL_OUTPUT_TOKENS, ProcessTraceWire, Tool, ToolContext, ToolDefinition, ToolError,
        ToolExecution, ToolExecutionWire, ToolInput, ToolInputError, ToolOutput, ToolOutputBody,
        ToolOutputContent, ToolOutputWire, ToolResult,
    };
}

#[cfg(target_family = "wasm")]
/// Code Mode results and observation contracts for the host-backed WASM runtime.
pub mod code_mode {
    pub use crate::wasm::{
        CodeModeExecution, CodeModeNotification, CodeModeObserver, CodeModeUpdate, NestedToolCall,
    };
}

#[cfg(target_family = "wasm")]
/// Image input and output preparation for the host-backed WASM runtime.
pub mod image {
    pub use crate::wasm::{prepare_output_images, prepare_user_input};
    pub use nanocodex_oai_api::ImageDetail;
}

#[cfg(target_family = "wasm")]
/// Host-backed tool selection and execution runtime.
pub mod runtime {
    pub use crate::wasm::{
        ImageGenerationConfig, OwnedToolContext, ToolRuntime, ToolRuntimeControl, Tools,
        WebSearchConfig,
    };
}

pub use contract::{Tool, ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult};
#[cfg(not(target_family = "wasm"))]
pub(crate) use contract::{ToolExecution, ToolOutputBody, ToolOutputContent};
#[cfg(not(target_family = "wasm"))]
pub(crate) use image::ImageDetail;
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub use nanocodex_tools_macros::tool;
pub use runtime::Tools;
#[cfg(not(target_family = "wasm"))]
pub(crate) use runtime::{DynamicToolProvider, ImageGenerationConfig, WebSearchConfig};
#[cfg(not(target_family = "wasm"))]
#[cfg_attr(docsrs, doc(cfg(not(target_family = "wasm"))))]
pub use runtime::{ToolsBuildError, ToolsBuilder};
#[cfg(not(target_family = "wasm"))]
pub(crate) use standard::StandardTool;

#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
pub mod __private {
    pub use async_trait::async_trait;
    pub use schemars;
    pub use serde;

    pub use crate::{
        Tool, ToolContext, ToolDefinition, ToolInput, ToolResult, contract::ToolExecution,
        runtime::schema_for,
    };
}
