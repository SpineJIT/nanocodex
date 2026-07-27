#![doc = include_str!("../README.md")]
#![deny(missing_docs, rustdoc::broken_intra_doc_links)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Authentication sources and managed credential snapshots.
pub mod auth;
/// Complete typed lifecycle events emitted around Responses operations.
pub mod events;
mod openai;
/// Automatic `gpt-5.6-sol` USD estimates from provider token usage.
pub mod pricing;
/// Complete typed request, event, and item model for the Responses protocol.
pub mod responses;
/// Managed session identities, inputs, checkpoints, and compaction results.
pub mod session;
/// Tool contracts shared by agent loops and concrete tool runtimes.
pub mod tools;
/// Generic Tower attempt, service, retry, and streamed-output contracts.
pub mod tower;
/// Responses transport policy, errors, and connection statistics.
pub mod transport;

use std::{fmt, path::PathBuf, str::FromStr, sync::Arc};

use serde::{Deserialize, Serialize};

pub(crate) use auth::{OpenAiAuth, OpenAiAuthError, OpenAiAuthMode, OpenAiAuthSnapshot};
pub(crate) use events::{
    AgentEventData, AgentEventKind, AssistantEvent, ContextEvent, EventError, EventSink,
    ModelEvent, ReasoningEvent, RunEvent, ToolEvent, TransportEvent, monotonic_now_ns,
};
pub use openai::{OpenAi, OpenAiBuilder, OpenAiError};
pub(crate) use pricing::{CostStatus, EstimatedUsdCost};
pub use responses::ResponseEvent;
pub(crate) use responses::{
    ContentItem, FunctionOutputBody, FunctionOutputContent, MessagePhase, MessageRole,
    ResponseItem, ResponseItemId, ToolDefinition, Usage,
};
pub use session::{
    CompletedResponse, Response, ResponseError, ResponseTurn, Session, SessionBuildError,
    SessionBuilder,
};
#[doc(hidden)]
pub use session::{compaction, context};
pub(crate) use tools::ToolOutputBody;
pub(crate) use tower::attempt::{
    ResponsesAttempt, ResponsesAttemptFactory, ResponsesOutput, ResponsesServiceResponse,
    TransportStats,
};
pub(crate) use tower::stream::{CompactionOutput, GenerationOutput};
pub(crate) use tower::{
    DefaultResponsesService, ResponsesClient, ResponsesRetryPolicy, ResponsesService,
    ResponsesServiceError,
};
#[doc(hidden)]
pub type CompactionResult = CompactionOutput;
#[doc(hidden)]
pub type TurnResult = GenerationOutput;
pub(crate) use transport::socket::EncodedRequest;
pub(crate) use transport::{ResponsesError, ResponsesHistory, ResponsesTransport, RetryAdvice};

pub(crate) use tower::{attempt, middleware, service, service_error, stream};
#[cfg(not(target_family = "wasm"))]
pub(crate) use transport::{connector, http};
pub(crate) use transport::{socket, telemetry};

const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

/// The single Responses model contract supported by this SDK.
pub const MODEL: &str = "gpt-5.6-sol";

/// Context-window size of the supported Responses model contract.
pub const CONTEXT_WINDOW_TOKENS: u64 = 272_000;

/// User input for one agent turn.
///
/// Session policy such as the filesystem workspace belongs to the agent
/// builder rather than an individual prompt.
#[doc(hidden)]
#[allow(missing_docs)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub instruction: PromptInput,
}

#[allow(missing_docs)]
impl Prompt {
    #[must_use]
    pub fn new(instruction: impl Into<String>) -> Self {
        Self {
            instruction: PromptInput::Text(instruction.into()),
        }
    }

    /// Creates a prompt from ordered text, image, and audio input items.
    #[must_use]
    pub fn content(input: impl IntoIterator<Item = UserInput>) -> Self {
        Self {
            instruction: PromptInput::Content(input.into_iter().collect()),
        }
    }
}

impl From<String> for Prompt {
    fn from(instruction: String) -> Self {
        Self::new(instruction)
    }
}

impl From<&str> for Prompt {
    fn from(instruction: &str) -> Self {
        Self::new(instruction)
    }
}

/// Ordered input for one agent turn.
#[doc(hidden)]
#[allow(missing_docs)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PromptInput {
    Text(String),
    Content(Vec<UserInput>),
}

#[allow(missing_docs)]
impl PromptInput {
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Content(items) => items.iter().map(UserInput::text_bytes).sum(),
        }
    }

    #[must_use]
    pub fn text_chars(&self) -> usize {
        match self {
            Self::Text(text) => text.chars().count(),
            Self::Content(items) => items.iter().map(UserInput::text_chars).sum(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) => text.trim().is_empty(),
            Self::Content(items) => items.is_empty() || items.iter().all(UserInput::is_empty),
        }
    }
}

impl From<String> for PromptInput {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for PromptInput {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

/// One ordered user-supplied prompt item.
#[doc(hidden)]
#[allow(missing_docs)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserInput {
    Text {
        text: String,
    },
    Image {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
    LocalImage {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
    Audio {
        audio_url: String,
    },
    LocalAudio {
        path: PathBuf,
    },
}

#[doc(hidden)]
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    Auto,
    Low,
    High,
    Original,
}

#[allow(missing_docs)]
impl UserInput {
    #[must_use]
    pub const fn text_bytes(&self) -> usize {
        match self {
            Self::Text { text } => text.len(),
            Self::Image { .. }
            | Self::LocalImage { .. }
            | Self::Audio { .. }
            | Self::LocalAudio { .. } => 0,
        }
    }

    #[must_use]
    pub fn text_chars(&self) -> usize {
        match self {
            Self::Text { text } => text.chars().count(),
            Self::Image { .. }
            | Self::LocalImage { .. }
            | Self::Audio { .. }
            | Self::LocalAudio { .. } => 0,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text { text } => text.trim().is_empty(),
            Self::Image { .. }
            | Self::LocalImage { .. }
            | Self::Audio { .. }
            | Self::LocalAudio { .. } => false,
        }
    }
}

/// OpenAI-specific settings for the deliberately single-provider nanocodex.
#[doc(hidden)]
#[derive(Clone)]
pub struct ModelConfig {
    /// Authentication source resolved for each transport connection.
    pub auth: OpenAiAuth,
    /// Reasoning execution mode.
    pub reasoning_mode: ReasoningMode,
    /// Requested reasoning effort.
    pub thinking: Thinking,
    /// Whether requests use priority processing.
    pub fast_mode: bool,
    /// Selected streaming transport.
    pub responses_transport: ResponsesTransport,
    /// Selected healthy-call history strategy.
    pub responses_history: ResponsesHistory,
    /// Whether the provider may retain response checkpoints.
    pub store_responses: bool,
    /// Responses WebSocket endpoint.
    pub websocket_url: String,
    /// Base URL used for HTTPS Responses calls and related endpoints.
    pub api_base_url: String,
    /// Immutable harness system prompt serialized before session instructions.
    pub system_prompt: Arc<str>,
}

impl ModelConfig {
    /// Returns the fixed orchestration mode sent to the supported model.
    #[must_use]
    pub const fn orchestration() -> &'static str {
        "local_code_mode"
    }

    /// Returns the immutable harness system prompt.
    #[must_use]
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Returns the `OpenAI` tool-search endpoint derived from the base URL.
    #[must_use]
    pub fn search_endpoint(&self) -> String {
        format!("{}/alpha/search", self.api_base_url.trim_end_matches('/'))
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            auth: OpenAiAuth::api_key(String::new()),
            reasoning_mode: ReasoningMode::default(),
            thinking: Thinking::default(),
            fast_mode: false,
            responses_transport: ResponsesTransport::default(),
            responses_history: ResponsesHistory::default(),
            store_responses: true,
            websocket_url: "wss://api.openai.com/v1/responses".to_owned(),
            api_base_url: "https://api.openai.com/v1".to_owned(),
            system_prompt: SYSTEM_PROMPT.into(),
        }
    }
}

/// Responses reasoning execution mode for the supported GPT-5.6 model family.
///
/// Standard mode preserves the default request behavior. Pro mode performs
/// additional model work before returning one final answer and can increase
/// latency and token usage independently of [`Thinking`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReasoningMode {
    /// Standard reasoning behavior.
    #[default]
    Standard,
    /// Pro reasoning behavior.
    Pro,
}

impl ReasoningMode {
    /// Returns the request value used by the Responses API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Pro => "pro",
        }
    }

    pub(crate) const fn request_value(self) -> Option<&'static str> {
        match self {
            Self::Standard => None,
            Self::Pro => Some("pro"),
        }
    }
}

impl fmt::Display for ReasoningMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ReasoningMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "standard" => Ok(Self::Standard),
            "pro" => Ok(Self::Pro),
            _ => Err(format!(
                "invalid reasoning mode {value:?}; expected standard or pro"
            )),
        }
    }
}

/// Requested model reasoning effort.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Thinking {
    /// Disable reasoning when supported.
    None,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    #[default]
    High,
    /// Extra-high reasoning effort.
    Xhigh,
    /// Maximum reasoning effort.
    Max,
}

impl Thinking {
    /// Returns the request value used by the Responses API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for Thinking {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Thinking {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => Err(format!(
                "invalid reasoning effort {value:?}; expected none, low, medium, high, xhigh, or max"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Prompt, ReasoningMode, Thinking};

    #[test]
    fn reasoning_configuration_parses_every_public_value() {
        assert_eq!("standard".parse(), Ok(ReasoningMode::Standard));
        assert_eq!("pro".parse(), Ok(ReasoningMode::Pro));

        for (value, expected) in [
            ("none", Thinking::None),
            ("low", Thinking::Low),
            ("medium", Thinking::Medium),
            ("high", Thinking::High),
            ("xhigh", Thinking::Xhigh),
            ("max", Thinking::Max),
        ] {
            assert_eq!(value.parse(), Ok(expected));
        }
    }

    #[test]
    fn prompt_serialization_contains_only_user_input() {
        let prompt = Prompt::new("inspect the repository");
        assert_eq!(
            serde_json::to_value(prompt).unwrap(),
            json!({ "instruction": "inspect the repository" })
        );
    }

    #[test]
    fn prompt_deserialization_rejects_session_policy() {
        let error = serde_json::from_value::<Prompt>(json!({
            "instruction": "inspect the repository",
            "workspace": "/work/project"
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field `workspace`"));
    }
}
