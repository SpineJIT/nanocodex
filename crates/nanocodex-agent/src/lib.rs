//! Owned lifecycle for one headless `OpenAI` coding agent.
//!
//! `nanocodex-agent` composes the Tower-native Responses state machine from
//! `nanocodex-oai-api` with the runtime from `nanocodex-tools`. A normal
//! consumer builds one agent, receives a cheap cloneable [`Nanocodex`] handle
//! and independent [`AgentEvents`] stream, then submits ordered prompts:
//!
//! ```no_run
//! use nanocodex_agent::{Nanocodex, OpenAi};
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
//!
//! The private driver is the sole owner of mutable conversation, transport,
//! tool, and process state. Cloning [`Nanocodex`] only clones its command
//! capability; [`Nanocodex::spawn`] creates a clean sibling and
//! [`Nanocodex::fork`] creates an independent branch from committed history.

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

extern crate self as nanocodex_agent;

#[cfg(not(target_family = "wasm"))]
mod agent;
#[cfg(not(target_family = "wasm"))]
mod auth;
mod error;
mod model;
mod prompt_cache;
#[cfg(not(target_family = "wasm"))]
mod responses;
#[cfg(not(target_family = "wasm"))]
mod rollout;
mod session;
mod usage;
#[cfg(target_family = "wasm")]
mod wasm;

#[cfg(not(target_family = "wasm"))]
pub use agent::{AgentHandle, Nanocodex, NanocodexBuilder, Turn, TurnControl, TurnResult};
#[cfg(not(target_family = "wasm"))]
pub use async_trait::async_trait;
#[cfg(not(target_family = "wasm"))]
pub use auth::{
    ChatGptAuthError, ChatGptAuthStatus, ChatGptLogin, chatgpt_auth_status, load_chatgpt_auth,
    logout_chatgpt,
};
pub use error::{NanocodexError, ResponsesError, Result};
pub use nanocodex_oai_api::OpenAi;
pub use nanocodex_oai_api::responses::RequestProfile;
pub use nanocodex_oai_api::{
    AgentEvent, AgentEventKind, AgentEventTiming, AgentEvents, AgentMessageContent, ContentItem,
    CustomToolFormat, DefaultResponsesService, FunctionOutputBody, FunctionOutputContent,
    ImageDetail, InternalMessageMetadata, ItemStatus, JsonSchema, JsonValue, LocalShellAction,
    LocalShellExecAction, LocalShellStatus, MODEL, MessagePhase, MessageRole, OpenAiAuth,
    OpenAiAuthError, OpenAiAuthMode, OpenAiError, OutputTextAnnotation, OutputTextLogprob,
    OutputTextTopLogprob, Prompt, PromptInput, ReasoningContent, ReasoningMode, ReasoningSummary,
    ResponseItem, ResponseItemId, ResponsesAttempt, ResponsesAttemptKind, ResponsesClient,
    ResponsesHistory, ResponsesRetryPolicy, ResponsesService, ResponsesServiceError,
    ResponsesServiceResponse, ResponsesTransport, SessionId, SessionIdError, Thinking,
    TimedAgentEvent, ToolCaller, ToolDefinition, Usage, UserInput, WebSearchAction,
    monotonic_now_ns,
};
#[cfg(not(target_family = "wasm"))]
pub use nanocodex_tools::tool;
#[cfg(not(target_family = "wasm"))]
pub use nanocodex_tools::{
    DEFAULT_TOOL_OUTPUT_TOKENS, Mcp, McpBuildError, McpBuilder, McpControlError, McpHandle,
    McpLogin, McpOAuthCredentials, McpOAuthStore, McpServer, StandardTool, Tool, ToolContext,
    ToolError, ToolExecution, ToolInput, ToolInputError, ToolOutput, ToolOutputBody,
    ToolOutputContent, ToolOutputWire, ToolResult, Tools, ToolsBuildError, ToolsBuilder,
    UpdatePlanTool,
};
#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
pub use responses::{FactoryResponses, LayeredResponses, OpenAiResponses, StandardResponses};
#[cfg(not(target_family = "wasm"))]
pub use responses::{Responses, ResponsesBuilder};
#[cfg(not(target_family = "wasm"))]
pub use rollout::{DurableSession, RolloutConfig, RolloutInfo, RolloutTranscriptItem};
#[cfg(not(target_family = "wasm"))]
pub use schemars::JsonSchema as ToolSchema;
pub use session::SessionSnapshot;
pub use usage::TurnUsage;
#[cfg(target_family = "wasm")]
pub use wasm::{WasmNanocodex, WasmTurn};

#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
pub mod __private {
    pub use async_trait::async_trait;
    pub use nanocodex_tools::schema_for;
    pub use schemars;
    pub use serde;
}
