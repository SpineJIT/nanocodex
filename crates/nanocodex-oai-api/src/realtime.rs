//! GPT Realtime WebSocket sessions.
//!
//! The transport deliberately stops at typed audio and conversation events.
//! Device capture/playback and delegation to a coding agent are application
//! concerns; this keeps the library usable with pipes and custom media stacks.

use std::{fmt, str::FromStr, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        Error as WebSocketError, Message,
        client::IntoClientRequest,
        http::{HeaderValue, header},
    },
};
use tracing::{debug, trace};
use url::Url;

use crate::{OpenAiAuth, OpenAiAuthError, OpenAiAuthMode, connector::connect_async};

mod webrtc;

/// Sample rate required for GPT Realtime PCM audio.
pub const REALTIME_SAMPLE_RATE: u32 = 24_000;
/// Channel count required for GPT Realtime PCM audio.
pub const REALTIME_CHANNELS: u16 = 1;
/// Default model used by native Realtime sessions.
pub const REALTIME_MODEL: &str = "gpt-realtime-1.5";
/// Default model used by ChatGPT-authenticated Codex voice sessions.
pub const CHATGPT_REALTIME_MODEL: &str = "gpt-live-1-boulder-alpha";

/// Voices supported by Codex's Frameless/V3 ChatGPT voice sessions.
pub const CHATGPT_REALTIME_VOICES: &[RealtimeVoice] = &[
    RealtimeVoice::Juniper,
    RealtimeVoice::Maple,
    RealtimeVoice::Spruce,
    RealtimeVoice::Ember,
    RealtimeVoice::Vale,
    RealtimeVoice::Breeze,
    RealtimeVoice::Arbor,
    RealtimeVoice::Sol,
    RealtimeVoice::Cove,
];

/// Default voice used by Codex's Frameless/V3 ChatGPT voice sessions.
pub const CHATGPT_REALTIME_VOICE: RealtimeVoice = RealtimeVoice::Cove;

/// Voices supported by direct Platform Realtime sessions.
pub const PLATFORM_REALTIME_VOICES: &[RealtimeVoice] = &[
    RealtimeVoice::Alloy,
    RealtimeVoice::Ash,
    RealtimeVoice::Ballad,
    RealtimeVoice::Coral,
    RealtimeVoice::Echo,
    RealtimeVoice::Sage,
    RealtimeVoice::Shimmer,
    RealtimeVoice::Verse,
    RealtimeVoice::Marin,
    RealtimeVoice::Cedar,
];

/// Default voice used by direct Platform Realtime sessions.
pub const PLATFORM_REALTIME_VOICE: RealtimeVoice = RealtimeVoice::Marin;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_CAPACITY: usize = 256;
const EVENT_CAPACITY: usize = 256;
const BACKGROUND_AGENT_TOOL: &str = "background_agent";
const BACKGROUND_AGENT_TOOL_DESCRIPTION: &str = "Send a user request to the background agent. Use this as the default action. Do not rephrase the user's ask or rewrite it in your own words; pass along the user's own words. If the background agent is idle, this starts a new task and returns the final result to the user. If the background agent is already working on a task, this sends the request as guidance to steer that previous task. If the user asks to do something next, later, after this, or once current work finishes, call this tool so the work is actually queued instead of merely promising to do it later.";
const REMAIN_SILENT_TOOL: &str = "remain_silent";
const REMAIN_SILENT_TOOL_DESCRIPTION: &str = "Call this when the best response is to say nothing. Use it instead of speaking after hidden system/control messages, after background agent updates in silent modes, or whenever acknowledging aloud would be distracting. This tool has no user-visible effect.";
const STEER_ACKNOWLEDGEMENT: &str = "This was sent to steer the previous background agent task.";
const AGENT_COMPLETE_ACKNOWLEDGEMENT: &str =
    "Background agent finished. Use the preceding [BACKEND] messages as the result.";
const BACKEND_TEXT_PREFIX: &str = "[BACKEND] ";
const CONTEXT_APPEND_MAX_BYTES: usize = 500;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A GPT Realtime output voice supported by the current realtime protocol.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum RealtimeVoice {
    /// Alloy voice.
    Alloy,
    /// Arbor voice.
    Arbor,
    /// Ash voice.
    Ash,
    /// Ballad voice.
    Ballad,
    /// Breeze voice.
    Breeze,
    /// Cedar voice.
    Cedar,
    /// Coral voice.
    Coral,
    /// Cove voice.
    Cove,
    /// Echo voice.
    Echo,
    /// Ember voice.
    Ember,
    /// Juniper voice.
    Juniper,
    /// Maple voice.
    Maple,
    /// Marin voice, the direct Platform default.
    #[default]
    Marin,
    /// Sage voice.
    Sage,
    /// Shimmer voice.
    Shimmer,
    /// Sol voice.
    Sol,
    /// Spruce voice.
    Spruce,
    /// Vale voice.
    Vale,
    /// Verse voice.
    Verse,
}

impl RealtimeVoice {
    /// Returns the protocol value for this voice.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Alloy => "alloy",
            Self::Arbor => "arbor",
            Self::Ash => "ash",
            Self::Ballad => "ballad",
            Self::Breeze => "breeze",
            Self::Cedar => "cedar",
            Self::Coral => "coral",
            Self::Cove => "cove",
            Self::Echo => "echo",
            Self::Ember => "ember",
            Self::Juniper => "juniper",
            Self::Maple => "maple",
            Self::Marin => "marin",
            Self::Sage => "sage",
            Self::Shimmer => "shimmer",
            Self::Sol => "sol",
            Self::Spruce => "spruce",
            Self::Vale => "vale",
            Self::Verse => "verse",
        }
    }

    const fn supports_frameless(self) -> bool {
        matches!(
            self,
            Self::Arbor
                | Self::Breeze
                | Self::Cove
                | Self::Ember
                | Self::Juniper
                | Self::Maple
                | Self::Sol
                | Self::Spruce
                | Self::Vale
        )
    }

    const fn supports_direct(self) -> bool {
        matches!(
            self,
            Self::Alloy
                | Self::Ash
                | Self::Ballad
                | Self::Cedar
                | Self::Coral
                | Self::Echo
                | Self::Marin
                | Self::Sage
                | Self::Shimmer
                | Self::Verse
        )
    }
}

impl fmt::Display for RealtimeVoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RealtimeVoice {
    type Err = RealtimeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "alloy" => Ok(Self::Alloy),
            "arbor" => Ok(Self::Arbor),
            "ash" => Ok(Self::Ash),
            "ballad" => Ok(Self::Ballad),
            "breeze" => Ok(Self::Breeze),
            "cedar" => Ok(Self::Cedar),
            "coral" => Ok(Self::Coral),
            "cove" => Ok(Self::Cove),
            "echo" => Ok(Self::Echo),
            "ember" => Ok(Self::Ember),
            "juniper" => Ok(Self::Juniper),
            "maple" => Ok(Self::Maple),
            "marin" => Ok(Self::Marin),
            "sage" => Ok(Self::Sage),
            "shimmer" => Ok(Self::Shimmer),
            "sol" => Ok(Self::Sol),
            "spruce" => Ok(Self::Spruce),
            "vale" => Ok(Self::Vale),
            "verse" => Ok(Self::Verse),
            _ => Err(RealtimeError::InvalidVoice(value.to_owned())),
        }
    }
}

/// One owned 24 kHz mono signed-16-bit little-endian PCM chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeAudio {
    data: Vec<u8>,
}

impl RealtimeAudio {
    /// Creates a PCM chunk from signed-16-bit little-endian bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the byte count does not contain complete samples.
    pub fn pcm16_le(data: impl Into<Vec<u8>>) -> Result<Self, RealtimeError> {
        let data = data.into();
        if data.len() % size_of::<i16>() != 0 {
            return Err(RealtimeError::InvalidAudio(
                "PCM16 audio must contain complete little-endian samples".to_owned(),
            ));
        }
        Ok(Self { data })
    }

    /// Creates a PCM chunk from native signed samples.
    #[must_use]
    pub fn from_samples(samples: impl IntoIterator<Item = i16>) -> Self {
        let samples = samples.into_iter();
        let mut data = Vec::with_capacity(samples.size_hint().0.saturating_mul(2));
        for sample in samples {
            data.extend_from_slice(&sample.to_le_bytes());
        }
        Self { data }
    }

    /// Returns the signed-16-bit little-endian PCM bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the chunk and returns its PCM bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    /// Returns the number of mono samples in this chunk.
    #[must_use]
    pub const fn samples(&self) -> usize {
        self.data.len() / size_of::<i16>()
    }

    /// Returns whether this chunk contains no samples.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// A typed event from a GPT Realtime conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RealtimeEvent {
    /// The server accepted the session configuration.
    SessionReady {
        /// Provider-assigned realtime session identity.
        session_id: String,
    },
    /// Voice activity detection observed new user speech.
    SpeechStarted,
    /// Incremental transcription of user speech.
    InputTranscriptDelta(String),
    /// Completed transcription of one user utterance.
    InputTranscriptDone(String),
    /// Incremental transcript of synthesized output speech.
    OutputTranscriptDelta(String),
    /// Completed transcript of synthesized output speech.
    OutputTranscriptDone(String),
    /// Synthesized 24 kHz mono PCM16 audio.
    Audio(RealtimeAudio),
    /// Realtime requested work from the background coding agent.
    AgentRequest {
        /// Function call identity to complete with [`RealtimeSession::complete_agent_request`].
        call_id: String,
        /// User request selected by the realtime model for delegation.
        prompt: String,
        /// Voice transcript entries added since the previous delegation.
        transcript: Vec<RealtimeTranscriptEntry>,
    },
    /// Realtime requested an intentionally silent tool result.
    RemainSilent {
        /// Function call identity to acknowledge with [`RealtimeSession::complete_silent_request`].
        call_id: String,
    },
    /// A realtime response began.
    ResponseStarted,
    /// A realtime response completed.
    ResponseDone,
    /// The provider reported a session error.
    Error(String),
}

/// One role-bearing voice transcript entry associated with a Realtime delegation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeTranscriptEntry {
    /// Transcript participant as the Realtime wire role (`user` or `assistant`).
    pub role: String,
    /// Complete transcript text for this contiguous role entry.
    pub text: String,
}

/// Receiver for the independent typed event stream of a realtime session.
pub struct RealtimeEvents {
    receiver: mpsc::Receiver<RealtimeEvent>,
}

impl RealtimeEvents {
    /// Waits for the next typed realtime event.
    pub async fn recv(&mut self) -> Option<RealtimeEvent> {
        self.receiver.recv().await
    }

    /// Attempts to receive an already-buffered event.
    pub fn try_recv(&mut self) -> Option<RealtimeEvent> {
        self.receiver.try_recv().ok()
    }
}

/// Cloneable command handle for one active GPT Realtime session.
#[derive(Clone)]
pub struct RealtimeSession {
    commands: mpsc::Sender<Command>,
    protocol: RealtimeProtocol,
}

/// Protocol-specific handling applied after live input steers an active agent turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealtimeAgentSteer {
    /// Realtime V2 received the steering tool result immediately.
    Acknowledged,
    /// Frameless moved the active delegation target to the newest request.
    ReplacedDelegation,
}

impl RealtimeSession {
    /// Returns whether this protocol accepts incremental background-agent appends.
    ///
    /// Codex streams agent message deltas into Frameless delegations. Realtime
    /// V2 instead receives each completed agent message as one `[BACKEND]`
    /// conversation item.
    #[must_use]
    pub const fn streams_agent_output(&self) -> bool {
        matches!(self.protocol, RealtimeProtocol::Frameless)
    }

    /// Appends one owned 24 kHz mono PCM16 input chunk.
    ///
    /// # Errors
    ///
    /// Returns an error when the session has closed or sending times out.
    pub async fn send_audio(&self, audio: RealtimeAudio) -> Result<(), RealtimeError> {
        self.send(CommandKind::Audio(audio)).await
    }

    /// Completes a background-agent request and asks Realtime to speak the result.
    ///
    /// # Errors
    ///
    /// Returns an error when the session has closed or sending times out.
    pub async fn complete_agent_request(
        &self,
        call_id: impl Into<String>,
        output: impl Into<String>,
    ) -> Result<(), RealtimeError> {
        self.send(CommandKind::AgentOutput {
            call_id: call_id.into(),
            output: output.into(),
        })
        .await
    }

    /// Applies Codex's protocol-specific acknowledgement for a steering request.
    ///
    /// Realtime V2 completes the new tool call with Codex's steering
    /// acknowledgement and creates a response. Frameless keeps the delegation
    /// open and makes the newest delegation item the target for subsequent
    /// background-agent output.
    ///
    /// # Errors
    ///
    /// Returns an error when the V2 acknowledgement cannot be delivered.
    pub async fn steer_agent_request(
        &self,
        call_id: impl Into<String>,
    ) -> Result<RealtimeAgentSteer, RealtimeError> {
        match self.protocol {
            RealtimeProtocol::Direct => {
                self.complete_agent_request(call_id, STEER_ACKNOWLEDGEMENT)
                    .await?;
                Ok(RealtimeAgentSteer::Acknowledged)
            }
            RealtimeProtocol::Frameless => {
                drop(call_id.into());
                Ok(RealtimeAgentSteer::ReplacedDelegation)
            }
        }
    }

    /// Appends streamed background-agent output to the active voice handoff.
    ///
    /// Realtime V2 receives a `[BACKEND]` user item. Frameless appends text to
    /// the active delegation context without asking for a separate response.
    ///
    /// # Errors
    ///
    /// Returns an error when the output cannot be delivered.
    pub async fn append_agent_output(
        &self,
        call_id: impl Into<String>,
        output: impl Into<String>,
    ) -> Result<(), RealtimeError> {
        self.send(CommandKind::AgentProgress {
            call_id: call_id.into(),
            output: output.into(),
        })
        .await
    }

    /// Completes a streamed background-agent handoff using Codex's protocol behavior.
    ///
    /// Realtime V2 completes the original tool call with Codex's completion
    /// acknowledgement and creates a response. Frameless requires no terminal
    /// wire item after the final delegation context append.
    ///
    /// # Errors
    ///
    /// Returns an error when the V2 completion cannot be delivered.
    pub async fn complete_agent_run(
        &self,
        call_id: impl Into<String>,
    ) -> Result<(), RealtimeError> {
        self.send(CommandKind::AgentComplete {
            call_id: call_id.into(),
        })
        .await
    }

    /// Completes a `remain_silent` request without creating spoken output.
    ///
    /// # Errors
    ///
    /// Returns an error when the session has closed or sending times out.
    pub async fn complete_silent_request(
        &self,
        call_id: impl Into<String>,
    ) -> Result<(), RealtimeError> {
        self.send(CommandKind::SilentOutput {
            call_id: call_id.into(),
        })
        .await
    }

    /// Closes the realtime WebSocket.
    ///
    /// # Errors
    ///
    /// Returns an error when the close command cannot be delivered.
    pub async fn close(&self) -> Result<(), RealtimeError> {
        self.send(CommandKind::Close).await
    }

    async fn send(&self, kind: CommandKind) -> Result<(), RealtimeError> {
        let (result, completed) = oneshot::channel();
        let command = Command { kind, result };
        timeout(SEND_TIMEOUT, self.commands.send(command))
            .await
            .map_err(|_| RealtimeError::SendTimeout)?
            .map_err(|_| RealtimeError::Closed)?;
        timeout(SEND_TIMEOUT, completed)
            .await
            .map_err(|_| RealtimeError::SendTimeout)?
            .map_err(|_| RealtimeError::Closed)?
    }
}

/// Builder for one independent GPT Realtime conversation.
pub struct RealtimeSessionBuilder {
    auth: OpenAiAuth,
    api_base_url: String,
    attestation_header: Option<Arc<str>>,
    websocket_url: Option<String>,
    instructions: Arc<str>,
    model: String,
    voice: Option<RealtimeVoice>,
    session_id: Option<String>,
}

impl RealtimeSessionBuilder {
    pub(crate) fn new(auth: OpenAiAuth, api_base_url: String, instructions: Arc<str>) -> Self {
        let model = match auth.mode() {
            OpenAiAuthMode::ApiKey => REALTIME_MODEL,
            OpenAiAuthMode::ChatGpt => CHATGPT_REALTIME_MODEL,
        };
        Self {
            auth,
            api_base_url,
            attestation_header: None,
            websocket_url: None,
            instructions,
            model: model.to_owned(),
            voice: None,
            session_id: None,
        }
    }

    /// Selects the GPT Realtime model.
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Selects the output voice.
    #[must_use]
    pub const fn voice(mut self, voice: RealtimeVoice) -> Self {
        self.voice = Some(voice);
        self
    }

    /// Replaces the derived Realtime WebSocket URL.
    #[must_use]
    pub fn websocket_url(mut self, websocket_url: impl Into<String>) -> Self {
        self.websocket_url = Some(websocket_url.into());
        self
    }

    /// Supplies a stable caller-owned session identity header.
    #[must_use]
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Supplies a host-generated `x-oai-attestation` value for ChatGPT calls.
    ///
    /// The value is opaque to Nanocodex and is reused only for the call and its
    /// sideband join. When omitted, Nanocodex sends the same unavailable-token
    /// envelope Codex uses when host attestation generation times out.
    #[must_use]
    pub fn attestation_header(mut self, value: impl Into<Arc<str>>) -> Self {
        self.attestation_header = Some(value.into());
        self
    }

    /// Connects and configures the realtime conversation.
    ///
    /// The returned command handle and event stream are independent. Dropping
    /// every command handle closes the socket task.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, authentication, timeout, or
    /// a failed WebSocket/WebRTC handshake.
    pub async fn connect(self) -> Result<(RealtimeSession, RealtimeEvents), RealtimeError> {
        if self.instructions.trim().is_empty() {
            return Err(RealtimeError::InvalidInstructions);
        }
        if self.model.trim().is_empty() {
            return Err(RealtimeError::InvalidModel);
        }

        let (socket, protocol, media) = match self.auth.mode() {
            OpenAiAuthMode::ApiKey => {
                let voice = self.voice.unwrap_or(PLATFORM_REALTIME_VOICE);
                if !voice.supports_direct() {
                    return Err(RealtimeError::InvalidVoice(voice.to_string()));
                }
                let auth = self.auth.snapshot().await?;
                let endpoint = match self.websocket_url {
                    Some(endpoint) => endpoint,
                    None => realtime_endpoint(&self.api_base_url, &self.model)?,
                };
                let mut request = endpoint
                    .as_str()
                    .into_client_request()
                    .map_err(|error| RealtimeError::InvalidUrl(error.to_string()))?;
                request.headers_mut().insert(
                    header::AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {}", auth.bearer()))
                        .map_err(|error| RealtimeError::InvalidAuthorization(error.to_string()))?,
                );
                request.headers_mut().insert(
                    header::USER_AGENT,
                    HeaderValue::from_static(concat!("nanocodex/", env!("CARGO_PKG_VERSION"))),
                );
                if let Some(session_id) = &self.session_id {
                    request.headers_mut().insert(
                        "x-session-id",
                        HeaderValue::from_str(session_id)
                            .map_err(|error| RealtimeError::InvalidSessionId(error.to_string()))?,
                    );
                }

                let connect_started = Instant::now();
                let (mut socket, response) = timeout(CONNECT_TIMEOUT, connect_async(request))
                    .await
                    .map_err(|_| RealtimeError::ConnectTimeout)?
                    .map_err(map_websocket_error)?;
                debug!(
                    status = response.status().as_u16(),
                    elapsed_ms = connect_started.elapsed().as_millis(),
                    "connected GPT Realtime websocket"
                );
                let update = session_update(&self.instructions, voice);
                send_json(&mut socket, &update).await?;
                (socket, RealtimeProtocol::Direct, None)
            }
            OpenAiAuthMode::ChatGpt => {
                let voice = self.voice.unwrap_or(CHATGPT_REALTIME_VOICE);
                if !voice.supports_frameless() {
                    return Err(RealtimeError::InvalidVoice(voice.to_string()));
                }
                let connection = webrtc::connect(webrtc::ConnectConfig {
                    auth: &self.auth,
                    api_base_url: &self.api_base_url,
                    attestation_header: self.attestation_header.as_deref(),
                    websocket_url: self.websocket_url.as_deref(),
                    instructions: &self.instructions,
                    model: &self.model,
                    voice,
                    session_id: self.session_id.as_deref(),
                })
                .await?;
                (
                    connection.socket,
                    RealtimeProtocol::Frameless,
                    Some(connection.media),
                )
            }
        };

        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        tokio::spawn(run_socket(socket, command_rx, event_tx, protocol, media));
        Ok((
            RealtimeSession {
                commands: command_tx,
                protocol,
            },
            RealtimeEvents { receiver: event_rx },
        ))
    }
}

/// Failure from configuring or operating a GPT Realtime session.
#[derive(Debug, thiserror::Error)]
pub enum RealtimeError {
    /// Managed credentials could not be resolved.
    #[error(transparent)]
    Authentication(#[from] OpenAiAuthError),
    /// Developer instructions were empty.
    #[error("GPT Realtime instructions must not be empty")]
    InvalidInstructions,
    /// The realtime model identifier was empty.
    #[error("GPT Realtime model must not be empty")]
    InvalidModel,
    /// The selected voice was not recognized.
    #[error("unsupported GPT Realtime voice {0:?}")]
    InvalidVoice(String),
    /// PCM input was malformed.
    #[error("invalid GPT Realtime audio: {0}")]
    InvalidAudio(String),
    /// The realtime URL was invalid.
    #[error("invalid GPT Realtime URL: {0}")]
    InvalidUrl(String),
    /// An authorization header could not be represented.
    #[error("invalid GPT Realtime authorization: {0}")]
    InvalidAuthorization(String),
    /// A caller-owned session header was invalid.
    #[error("invalid GPT Realtime session ID: {0}")]
    InvalidSessionId(String),
    /// Connecting exceeded the transport deadline.
    #[error("GPT Realtime connection timed out")]
    ConnectTimeout,
    /// Sending exceeded the transport deadline.
    #[error("GPT Realtime send timed out")]
    SendTimeout,
    /// The realtime session is closed.
    #[error("GPT Realtime session is closed")]
    Closed,
    /// A WebSocket operation failed.
    #[error("GPT Realtime WebSocket failed: {0}")]
    WebSocket(String),
    /// Creating the authenticated Realtime call failed.
    #[error("GPT Realtime HTTP call failed: {0}")]
    Http(String),
    /// Negotiating or decoding Realtime WebRTC media failed.
    #[error("GPT Realtime WebRTC failed: {0}")]
    WebRtc(String),
    /// A realtime JSON message could not be encoded or decoded.
    #[error("invalid GPT Realtime message: {0}")]
    Message(String),
}

struct Command {
    kind: CommandKind,
    result: oneshot::Sender<Result<(), RealtimeError>>,
}

enum CommandKind {
    Audio(RealtimeAudio),
    AgentOutput { call_id: String, output: String },
    AgentProgress { call_id: String, output: String },
    AgentComplete { call_id: String },
    SilentOutput { call_id: String },
    Close,
}

#[derive(Clone, Copy)]
enum RealtimeProtocol {
    Direct,
    Frameless,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ClientEvent<'a> {
    #[serde(rename = "session.update")]
    SessionUpdate { session: SessionUpdate<'a> },
    #[serde(rename = "input_audio_buffer.append")]
    AudioBufferAppend { audio: String },
    #[serde(rename = "conversation.item.create")]
    ItemCreate { item: ConversationItem<'a> },
    #[serde(rename = "response.create")]
    ResponseCreate,
    #[serde(rename = "delegation.context.append")]
    DelegationContextAppend {
        delegation_item_id: &'a str,
        content: [FramelessInputText<'a>; 1],
    },
    #[serde(rename = "session.close")]
    SessionClose,
}

#[derive(Serialize)]
struct FramelessInputText<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Serialize)]
struct SessionUpdate<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'a str,
    output_modalities: [&'static str; 1],
    audio: SessionAudio<'a>,
    tools: [SessionTool; 2],
    tool_choice: &'static str,
}

#[derive(Serialize)]
struct SessionAudio<'a> {
    input: SessionAudioInput,
    output: SessionAudioOutput<'a>,
}

#[derive(Serialize)]
struct SessionAudioInput {
    format: AudioFormat,
    noise_reduction: NoiseReduction,
    transcription: Transcription,
    turn_detection: TurnDetection,
}

#[derive(Serialize)]
struct SessionAudioOutput<'a> {
    format: AudioFormat,
    voice: &'a str,
}

#[derive(Clone, Copy, Serialize)]
struct AudioFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    rate: u32,
}

#[derive(Serialize)]
struct NoiseReduction {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct Transcription {
    model: &'static str,
}

#[derive(Serialize)]
struct TurnDetection {
    #[serde(rename = "type")]
    kind: &'static str,
    interrupt_response: bool,
    create_response: bool,
    silence_duration_ms: u32,
}

#[derive(Serialize)]
struct SessionTool {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ConversationItem<'a> {
    Message {
        #[serde(rename = "type")]
        kind: &'static str,
        role: &'static str,
        content: [ConversationInputText<'a>; 1],
    },
    FunctionOutput {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: &'a str,
        output: &'a str,
    },
}

#[derive(Serialize)]
struct ConversationInputText<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

fn session_update(instructions: &str, voice: RealtimeVoice) -> ClientEvent<'_> {
    let format = AudioFormat {
        kind: "audio/pcm",
        rate: REALTIME_SAMPLE_RATE,
    };
    ClientEvent::SessionUpdate {
        session: SessionUpdate {
            kind: "realtime",
            instructions,
            output_modalities: ["audio"],
            audio: SessionAudio {
                input: SessionAudioInput {
                    format,
                    noise_reduction: NoiseReduction { kind: "near_field" },
                    transcription: Transcription {
                        model: "gpt-4o-mini-transcribe",
                    },
                    turn_detection: TurnDetection {
                        kind: "server_vad",
                        interrupt_response: true,
                        create_response: true,
                        silence_duration_ms: 500,
                    },
                },
                output: SessionAudioOutput {
                    format,
                    voice: voice.as_str(),
                },
            },
            tools: [
                SessionTool {
                    kind: "function",
                    name: BACKGROUND_AGENT_TOOL,
                    description: BACKGROUND_AGENT_TOOL_DESCRIPTION,
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "prompt": { "type": "string" }
                        },
                        "required": ["prompt"],
                        "additionalProperties": false
                    }),
                },
                SessionTool {
                    kind: "function",
                    name: REMAIN_SILENT_TOOL,
                    description: REMAIN_SILENT_TOOL_DESCRIPTION,
                    parameters: json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                },
            ],
            tool_choice: "auto",
        },
    }
}

async fn run_socket(
    mut socket: Socket,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<RealtimeEvent>,
    protocol: RealtimeProtocol,
    mut media: Option<webrtc::WebRtcMedia>,
) {
    let media_input = media.as_ref().map(webrtc::WebRtcMedia::input);
    let mut active_transcript = ActiveTranscript::default();
    let mut response_create = ResponseCreateQueue::default();
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    if let Err(error) = close_socket(&mut socket, protocol).await {
                        debug!(%error, "failed to close dropped GPT Realtime session");
                    }
                    break;
                };
                let result = handle_command(
                    &mut socket,
                    command.kind,
                    protocol,
                    media_input.as_ref(),
                    &mut response_create,
                ).await;
                let should_close = matches!(result, Ok(true));
                if let Err(error) = &result {
                    let _ = events.send(RealtimeEvent::Error(error.to_string())).await;
                }
                let failed = result.is_err();
                let _ = command.result.send(result.map(|_| ()));
                if should_close || failed {
                    break;
                }
            }
            message = socket.next() => {
                match handle_server_message(
                    &mut socket,
                    message,
                    &events,
                    protocol,
                    &mut active_transcript,
                    &mut response_create,
                ).await {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        let _ = events.send(RealtimeEvent::Error(error.to_string())).await;
                        break;
                    }
                }
            }
            audio = recv_media(&mut media) => {
                match audio {
                    Some(Ok(audio)) => {
                        if events.send(RealtimeEvent::Audio(audio)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        let _ = events.send(RealtimeEvent::Error(error.to_string())).await;
                        break;
                    }
                    None => media = None,
                }
            }
        }
    }
    if let Some(media) = media {
        media.close().await;
    }
    debug!("GPT Realtime websocket task stopped");
}

async fn recv_media(
    media: &mut Option<webrtc::WebRtcMedia>,
) -> Option<Result<RealtimeAudio, RealtimeError>> {
    match media {
        Some(media) => media.recv().await,
        None => std::future::pending().await,
    }
}

async fn handle_command(
    socket: &mut Socket,
    command: CommandKind,
    protocol: RealtimeProtocol,
    media_input: Option<&mpsc::Sender<RealtimeAudio>>,
    response_create: &mut ResponseCreateQueue,
) -> Result<bool, RealtimeError> {
    match command {
        CommandKind::Audio(audio) => {
            if !audio.is_empty() {
                match protocol {
                    RealtimeProtocol::Direct => {
                        send_json(
                            socket,
                            &ClientEvent::AudioBufferAppend {
                                audio: STANDARD.encode(audio.as_bytes()),
                            },
                        )
                        .await?;
                    }
                    RealtimeProtocol::Frameless => {
                        let input = media_input.ok_or_else(|| {
                            RealtimeError::WebRtc(
                                "frameless session omitted its microphone track".to_owned(),
                            )
                        })?;
                        timeout(SEND_TIMEOUT, input.send(audio))
                            .await
                            .map_err(|_| RealtimeError::SendTimeout)?
                            .map_err(|_| RealtimeError::Closed)?;
                    }
                }
            }
            Ok(false)
        }
        CommandKind::AgentOutput { call_id, output } => {
            match protocol {
                RealtimeProtocol::Direct => {
                    send_function_output(socket, &call_id, &output).await?;
                    response_create.request(socket).await?;
                }
                RealtimeProtocol::Frameless => {
                    send_delegation_context(socket, &call_id, &output).await?;
                }
            }
            Ok(false)
        }
        CommandKind::AgentProgress { call_id, output } => {
            match protocol {
                RealtimeProtocol::Direct => {
                    send_backend_output(socket, &output).await?;
                }
                RealtimeProtocol::Frameless => {
                    send_delegation_context(socket, &call_id, &output).await?;
                }
            }
            Ok(false)
        }
        CommandKind::AgentComplete { call_id } => {
            if matches!(protocol, RealtimeProtocol::Direct) {
                send_function_output(socket, &call_id, AGENT_COMPLETE_ACKNOWLEDGEMENT).await?;
                response_create.request(socket).await?;
            }
            Ok(false)
        }
        CommandKind::SilentOutput { call_id } => {
            match protocol {
                RealtimeProtocol::Direct => send_function_output(socket, &call_id, "").await?,
                RealtimeProtocol::Frameless => {
                    send_delegation_context(socket, &call_id, "").await?;
                }
            }
            Ok(false)
        }
        CommandKind::Close => {
            close_socket(socket, protocol).await?;
            Ok(true)
        }
    }
}

async fn close_socket(
    socket: &mut Socket,
    protocol: RealtimeProtocol,
) -> Result<(), RealtimeError> {
    if matches!(protocol, RealtimeProtocol::Frameless) {
        send_json(socket, &ClientEvent::SessionClose).await?;
    }
    socket.close(None).await.map_err(map_websocket_error)
}

async fn send_delegation_context(
    socket: &mut Socket,
    delegation_item_id: &str,
    output: &str,
) -> Result<(), RealtimeError> {
    for chunk in context_append_chunks(output) {
        send_json(
            socket,
            &ClientEvent::DelegationContextAppend {
                delegation_item_id,
                content: [FramelessInputText {
                    kind: "input_text",
                    text: chunk,
                }],
            },
        )
        .await?;
    }
    Ok(())
}

fn context_append_chunks(text: &str) -> Vec<&str> {
    if text.len() <= CONTEXT_APPEND_MAX_BYTES {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + CONTEXT_APPEND_MAX_BYTES).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

async fn send_function_output(
    socket: &mut Socket,
    call_id: &str,
    output: &str,
) -> Result<(), RealtimeError> {
    send_json(
        socket,
        &ClientEvent::ItemCreate {
            item: ConversationItem::FunctionOutput {
                kind: "function_call_output",
                call_id,
                output,
            },
        },
    )
    .await
}

async fn send_backend_output(socket: &mut Socket, output: &str) -> Result<(), RealtimeError> {
    let text = format!("{BACKEND_TEXT_PREFIX}{output}");
    send_json(
        socket,
        &ClientEvent::ItemCreate {
            item: ConversationItem::Message {
                kind: "message",
                role: "user",
                content: [ConversationInputText {
                    kind: "input_text",
                    text: &text,
                }],
            },
        },
    )
    .await
}

async fn send_json(socket: &mut Socket, value: &ClientEvent<'_>) -> Result<(), RealtimeError> {
    let payload =
        serde_json::to_string(value).map_err(|error| RealtimeError::Message(error.to_string()))?;
    trace!(target: "nanocodex_oai_api::realtime::wire", payload = %payload, "GPT Realtime request");
    timeout(SEND_TIMEOUT, socket.send(Message::Text(payload.into())))
        .await
        .map_err(|_| RealtimeError::SendTimeout)?
        .map_err(map_websocket_error)
}

async fn handle_server_message(
    socket: &mut Socket,
    message: Option<Result<Message, WebSocketError>>,
    events: &mpsc::Sender<RealtimeEvent>,
    protocol: RealtimeProtocol,
    active_transcript: &mut ActiveTranscript,
    response_create: &mut ResponseCreateQueue,
) -> Result<bool, RealtimeError> {
    let Some(message) = message else {
        return Ok(true);
    };
    match message.map_err(map_websocket_error)? {
        Message::Text(payload) => {
            trace!(target: "nanocodex_oai_api::realtime::wire", payload = %payload, "GPT Realtime event");
            if let Some(mut event) = parse_event(&payload, protocol)? {
                active_transcript.update(&mut event);
                if matches!(protocol, RealtimeProtocol::Direct) {
                    match &event {
                        RealtimeEvent::ResponseStarted => response_create.mark_started(),
                        RealtimeEvent::ResponseDone => {
                            response_create.mark_finished(socket).await?
                        }
                        _ => {}
                    }
                }
                if events.send(event).await.is_err() {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Message::Ping(payload) => {
            socket
                .send(Message::Pong(payload))
                .await
                .map_err(map_websocket_error)?;
            Ok(false)
        }
        Message::Pong(_) | Message::Frame(_) => Ok(false),
        Message::Close(_) => Ok(true),
        Message::Binary(_) => Err(RealtimeError::Message(
            "unexpected binary WebSocket frame".to_owned(),
        )),
    }
}

#[derive(Default)]
struct ResponseCreateQueue {
    active: bool,
    pending: bool,
}

impl ResponseCreateQueue {
    async fn request(&mut self, socket: &mut Socket) -> Result<(), RealtimeError> {
        if self.active {
            self.pending = true;
            return Ok(());
        }
        send_json(socket, &ClientEvent::ResponseCreate).await?;
        self.active = true;
        Ok(())
    }

    const fn mark_started(&mut self) {
        self.active = true;
    }

    async fn mark_finished(&mut self, socket: &mut Socket) -> Result<(), RealtimeError> {
        self.active = false;
        if !self.pending {
            return Ok(());
        }
        self.pending = false;
        self.request(socket).await
    }
}

#[derive(Default)]
struct ActiveTranscript {
    entries: Vec<RealtimeTranscriptEntry>,
    last_handoff_entry_count: usize,
    new_input_entry: bool,
    new_output_entry: bool,
}

impl ActiveTranscript {
    fn update(&mut self, event: &mut RealtimeEvent) {
        match event {
            RealtimeEvent::SpeechStarted => self.new_input_entry = true,
            RealtimeEvent::InputTranscriptDelta(delta) => {
                append_transcript_delta(&mut self.entries, "user", delta, self.new_input_entry);
                self.new_input_entry = false;
            }
            RealtimeEvent::OutputTranscriptDelta(delta) => {
                append_transcript_delta(
                    &mut self.entries,
                    "assistant",
                    delta,
                    self.new_output_entry,
                );
                self.new_output_entry = false;
            }
            RealtimeEvent::InputTranscriptDone(text) => {
                apply_transcript_done(&mut self.entries, "user", text, self.new_input_entry);
                self.new_input_entry = false;
            }
            RealtimeEvent::OutputTranscriptDone(text) => {
                apply_transcript_done(&mut self.entries, "assistant", text, self.new_output_entry);
                self.new_output_entry = false;
            }
            RealtimeEvent::AgentRequest {
                prompt, transcript, ..
            } => {
                append_handoff_input(&mut self.entries, prompt);
                *transcript = self.entries[self.last_handoff_entry_count..].to_vec();
                self.last_handoff_entry_count = self.entries.len();
                self.new_input_entry = true;
                self.new_output_entry = true;
            }
            RealtimeEvent::ResponseStarted => self.new_output_entry = true,
            RealtimeEvent::SessionReady { .. }
            | RealtimeEvent::Audio(_)
            | RealtimeEvent::RemainSilent { .. }
            | RealtimeEvent::ResponseDone
            | RealtimeEvent::Error(_) => {}
        }
    }
}

fn append_transcript_delta(
    entries: &mut Vec<RealtimeTranscriptEntry>,
    role: &str,
    delta: &str,
    force_new: bool,
) {
    if delta.is_empty() {
        return;
    }
    if !force_new
        && let Some(last) = entries.last_mut()
        && last.role == role
    {
        last.text.push_str(delta);
        return;
    }
    entries.push(RealtimeTranscriptEntry {
        role: role.to_owned(),
        text: delta.to_owned(),
    });
}

fn apply_transcript_done(
    entries: &mut Vec<RealtimeTranscriptEntry>,
    role: &str,
    text: &str,
    force_new: bool,
) {
    if text.is_empty() {
        return;
    }
    if !force_new
        && let Some(last) = entries.last_mut()
        && last.role == role
    {
        last.text = text.to_owned();
        return;
    }
    entries.push(RealtimeTranscriptEntry {
        role: role.to_owned(),
        text: text.to_owned(),
    });
}

fn append_handoff_input(entries: &mut Vec<RealtimeTranscriptEntry>, input: &str) {
    let input = input.trim();
    if input.is_empty()
        || entries
            .iter()
            .any(|entry| entry.role == "user" && entry.text.trim() == input)
    {
        return;
    }
    entries.push(RealtimeTranscriptEntry {
        role: "user".to_owned(),
        text: input.to_owned(),
    });
}

fn parse_event(
    payload: &str,
    protocol: RealtimeProtocol,
) -> Result<Option<RealtimeEvent>, RealtimeError> {
    let value: Value =
        serde_json::from_str(payload).map_err(|error| RealtimeError::Message(error.to_string()))?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let event = match protocol {
        RealtimeProtocol::Direct => parse_direct_event(&value, kind)?,
        RealtimeProtocol::Frameless => parse_frameless_event(&value, kind),
    };
    Ok(event)
}

fn parse_direct_event(value: &Value, kind: &str) -> Result<Option<RealtimeEvent>, RealtimeError> {
    let event = match kind {
        "session.updated" => {
            value
                .pointer("/session/id")
                .and_then(Value::as_str)
                .map(|session_id| RealtimeEvent::SessionReady {
                    session_id: session_id.to_owned(),
                })
        }
        "input_audio_buffer.speech_started" => Some(RealtimeEvent::SpeechStarted),
        "conversation.input_transcript.delta" => {
            string_field(value, "delta").map(RealtimeEvent::InputTranscriptDelta)
        }
        "conversation.input_transcript.turn_marked" => {
            string_field(value, "transcript").map(RealtimeEvent::InputTranscriptDone)
        }
        "conversation.item.input_audio_transcription.delta" => {
            string_field(value, "delta").map(RealtimeEvent::InputTranscriptDelta)
        }
        "conversation.item.input_audio_transcription.completed" => {
            string_field(value, "transcript").map(RealtimeEvent::InputTranscriptDone)
        }
        "response.output_text.delta" | "response.output_audio_transcript.delta" => {
            string_field(value, "delta").map(RealtimeEvent::OutputTranscriptDelta)
        }
        "conversation.output_transcript.delta" => {
            string_field(value, "delta").map(RealtimeEvent::OutputTranscriptDelta)
        }
        "response.output_text.done" => {
            string_field(value, "text").map(RealtimeEvent::OutputTranscriptDone)
        }
        "response.output_audio_transcript.done" => {
            string_field(value, "transcript").map(RealtimeEvent::OutputTranscriptDone)
        }
        "response.output_audio.delta" | "response.audio.delta" => {
            let encoded = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let data = STANDARD
                .decode(encoded)
                .map_err(|error| RealtimeError::Message(error.to_string()))?;
            Some(RealtimeEvent::Audio(RealtimeAudio::pcm16_le(data)?))
        }
        "conversation.item.done" => parse_completed_item(value),
        "response.created" => Some(RealtimeEvent::ResponseStarted),
        "response.done" | "response.cancelled" => Some(RealtimeEvent::ResponseDone),
        "error" => Some(RealtimeEvent::Error(
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown GPT Realtime error")
                .to_owned(),
        )),
        _ => None,
    };
    Ok(event)
}

fn parse_frameless_event(value: &Value, kind: &str) -> Option<RealtimeEvent> {
    match kind {
        "session.started" | "session.updated" => value
            .pointer("/session/id")
            .and_then(Value::as_str)
            .map(|session_id| RealtimeEvent::SessionReady {
                session_id: session_id.to_owned(),
            }),
        "input_transcript.added" => value
            .pointer("/item/text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .map(RealtimeEvent::InputTranscriptDelta),
        "output_transcript.added" => value
            .pointer("/item/text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .map(RealtimeEvent::OutputTranscriptDelta),
        "turn.done" => parse_frameless_turn(value),
        "delegation.created" => parse_frameless_delegation(value),
        // WebRTC media owns frameless output audio. Ignoring its sideband
        // mirror prevents duplicate playback.
        "output_audio.delta" => None,
        "error" => Some(parse_error(value)),
        _ => None,
    }
}

fn parse_frameless_turn(value: &Value) -> Option<RealtimeEvent> {
    let role = value.pointer("/turn/role")?.as_str()?;
    let transcript = value.pointer("/turn/transcript")?.as_str()?.to_owned();
    match role {
        "user" => Some(RealtimeEvent::InputTranscriptDone(transcript)),
        "assistant" => Some(RealtimeEvent::OutputTranscriptDone(transcript)),
        _ => None,
    }
}

fn parse_frameless_delegation(value: &Value) -> Option<RealtimeEvent> {
    let item = value.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("delegation")
        || item.get("target").and_then(Value::as_str) != Some("client")
    {
        return None;
    }
    let prompt = item
        .get("content")?
        .as_array()?
        .iter()
        .filter(|content| content.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<String>();
    Some(RealtimeEvent::AgentRequest {
        call_id: item.get("id")?.as_str()?.to_owned(),
        prompt,
        transcript: Vec::new(),
    })
}

fn parse_error(value: &Value) -> RealtimeEvent {
    RealtimeEvent::Error(
        value
            .pointer("/error/message")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown GPT Realtime error")
            .to_owned(),
    )
}

fn parse_completed_item(value: &Value) -> Option<RealtimeEvent> {
    let item = value.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }
    let name = item.get("name").and_then(Value::as_str)?;
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)?
        .to_owned();
    match name {
        BACKGROUND_AGENT_TOOL => Some(RealtimeEvent::AgentRequest {
            call_id,
            prompt: delegated_prompt(item.get("arguments").and_then(Value::as_str)),
            transcript: Vec::new(),
        }),
        REMAIN_SILENT_TOOL => Some(RealtimeEvent::RemainSilent { call_id }),
        _ => None,
    }
}

fn delegated_prompt(arguments: Option<&str>) -> String {
    let Some(arguments) = arguments else {
        return String::new();
    };
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            ["prompt", "input_transcript", "input", "text", "query"]
                .into_iter()
                .find_map(|key| value.get(key).and_then(Value::as_str).map(str::trim))
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| arguments.to_owned())
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn realtime_endpoint(api_base_url: &str, model: &str) -> Result<String, RealtimeError> {
    let mut url =
        Url::parse(api_base_url).map_err(|error| RealtimeError::InvalidUrl(error.to_string()))?;
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|()| RealtimeError::InvalidUrl("could not select wss".to_owned()))?,
        "http" => url
            .set_scheme("ws")
            .map_err(|()| RealtimeError::InvalidUrl("could not select ws".to_owned()))?,
        "wss" | "ws" => {}
        scheme => {
            return Err(RealtimeError::InvalidUrl(format!(
                "unsupported URL scheme {scheme}"
            )));
        }
    }
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/realtime") {
        url.set_path(&format!("{path}/realtime"));
    }
    url.query_pairs_mut().append_pair("model", model);
    Ok(url.into())
}

fn map_websocket_error(error: WebSocketError) -> RealtimeError {
    RealtimeError::WebSocket(error.to_string())
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use super::{
        ActiveTranscript, CHATGPT_REALTIME_MODEL, CHATGPT_REALTIME_VOICE, CHATGPT_REALTIME_VOICES,
        PLATFORM_REALTIME_VOICE, PLATFORM_REALTIME_VOICES, RealtimeAgentSteer, RealtimeAudio,
        RealtimeEvent, RealtimeProtocol, RealtimeTranscriptEntry, RealtimeVoice,
        context_append_chunks, delegated_prompt, parse_event, realtime_endpoint, session_update,
    };
    use crate::OpenAi;

    #[test]
    fn derives_realtime_endpoint_from_api_base() {
        assert_eq!(
            realtime_endpoint("https://api.openai.com/v1", "gpt-realtime-1.5").unwrap(),
            "wss://api.openai.com/v1/realtime?model=gpt-realtime-1.5"
        );
    }

    #[test]
    fn matches_codex_voice_catalog_and_defaults() {
        assert_eq!(CHATGPT_REALTIME_MODEL, "gpt-live-1-boulder-alpha");
        assert_eq!(CHATGPT_REALTIME_VOICE, RealtimeVoice::Cove);
        assert_eq!(PLATFORM_REALTIME_VOICE, RealtimeVoice::Marin);
        assert_eq!(
            CHATGPT_REALTIME_VOICES,
            &[
                RealtimeVoice::Juniper,
                RealtimeVoice::Maple,
                RealtimeVoice::Spruce,
                RealtimeVoice::Ember,
                RealtimeVoice::Vale,
                RealtimeVoice::Breeze,
                RealtimeVoice::Arbor,
                RealtimeVoice::Sol,
                RealtimeVoice::Cove,
            ]
        );
        assert_eq!(
            PLATFORM_REALTIME_VOICES,
            &[
                RealtimeVoice::Alloy,
                RealtimeVoice::Ash,
                RealtimeVoice::Ballad,
                RealtimeVoice::Coral,
                RealtimeVoice::Echo,
                RealtimeVoice::Sage,
                RealtimeVoice::Shimmer,
                RealtimeVoice::Verse,
                RealtimeVoice::Marin,
                RealtimeVoice::Cedar,
            ]
        );
    }

    #[test]
    fn session_update_uses_pcm_and_background_agent_tool() {
        let value =
            serde_json::to_value(session_update("delegate coding work", RealtimeVoice::Cove))
                .unwrap();
        assert_eq!(value["session"]["audio"]["input"]["format"]["rate"], 24_000);
        assert_eq!(value["session"]["audio"]["output"]["voice"], "cove");
        assert_eq!(value["session"]["tools"][0]["name"], "background_agent");
    }

    #[test]
    fn parses_audio_and_background_agent_events() {
        let audio = parse_event(
            r#"{"type":"response.output_audio.delta","delta":"AAE="}"#,
            RealtimeProtocol::Direct,
        )
        .unwrap();
        assert_eq!(
            audio,
            Some(RealtimeEvent::Audio(
                RealtimeAudio::pcm16_le([0, 1]).unwrap()
            ))
        );

        let request = parse_event(
            r#"{"type":"conversation.item.done","item":{"type":"function_call","name":"background_agent","call_id":"call_1","arguments":"{\"prompt\":\"inspect the tests\"}"}}"#,
            RealtimeProtocol::Direct,
        )
        .unwrap();
        assert_eq!(
            request,
            Some(RealtimeEvent::AgentRequest {
                call_id: "call_1".to_owned(),
                prompt: "inspect the tests".to_owned(),
                transcript: Vec::new(),
            })
        );
    }

    #[test]
    fn parses_frameless_transcripts_delegation_and_ignores_sideband_audio() {
        assert_eq!(
            parse_event(
                r#"{"type":"delegation.created","item":{"type":"delegation","target":"client","id":"delegation_1","content":[{"type":"input_text","text":"run "},{"type":"output_text","text":"ignored"},{"type":"input_text","text":"the tests"}]}}"#,
                RealtimeProtocol::Frameless,
            )
            .unwrap(),
            Some(RealtimeEvent::AgentRequest {
                call_id: "delegation_1".to_owned(),
                prompt: "run the tests".to_owned(),
                transcript: Vec::new(),
            })
        );
        assert_eq!(
            parse_event(
                r#"{"type":"output_audio.delta","audio":"AAE="}"#,
                RealtimeProtocol::Frameless,
            )
            .unwrap(),
            None
        );
        assert_eq!(
            parse_event(
                r#"{"type":"input_transcript.added","item":{"text":"hello"}}"#,
                RealtimeProtocol::Frameless,
            )
            .unwrap(),
            Some(RealtimeEvent::InputTranscriptDelta("hello".to_owned()))
        );
        assert_eq!(
            parse_event(
                r#"{"type":"turn.done","turn":{"role":"assistant","transcript":"all done"}}"#,
                RealtimeProtocol::Frameless,
            )
            .unwrap(),
            Some(RealtimeEvent::OutputTranscriptDone("all done".to_owned()))
        );
    }

    #[test]
    fn attaches_only_new_active_transcript_to_each_delegation() {
        let mut transcript = ActiveTranscript::default();
        transcript.update(&mut RealtimeEvent::InputTranscriptDelta(
            "delegate ".to_owned(),
        ));
        transcript.update(&mut RealtimeEvent::InputTranscriptDone(
            "delegate this".to_owned(),
        ));
        let mut first = RealtimeEvent::AgentRequest {
            call_id: "call_1".to_owned(),
            prompt: "delegate this".to_owned(),
            transcript: Vec::new(),
        };
        transcript.update(&mut first);
        assert_eq!(
            first,
            RealtimeEvent::AgentRequest {
                call_id: "call_1".to_owned(),
                prompt: "delegate this".to_owned(),
                transcript: vec![RealtimeTranscriptEntry {
                    role: "user".to_owned(),
                    text: "delegate this".to_owned(),
                }],
            }
        );

        transcript.update(&mut RealtimeEvent::OutputTranscriptDone(
            "On it.".to_owned(),
        ));
        transcript.update(&mut RealtimeEvent::InputTranscriptDone(
            "also run tests".to_owned(),
        ));
        let mut second = RealtimeEvent::AgentRequest {
            call_id: "call_2".to_owned(),
            prompt: "also run tests".to_owned(),
            transcript: Vec::new(),
        };
        transcript.update(&mut second);
        assert_eq!(
            second,
            RealtimeEvent::AgentRequest {
                call_id: "call_2".to_owned(),
                prompt: "also run tests".to_owned(),
                transcript: vec![
                    RealtimeTranscriptEntry {
                        role: "assistant".to_owned(),
                        text: "On it.".to_owned(),
                    },
                    RealtimeTranscriptEntry {
                        role: "user".to_owned(),
                        text: "also run tests".to_owned(),
                    },
                ],
            }
        );
    }

    #[test]
    fn accepts_piped_pcm_bytes_and_fallback_arguments() {
        assert_eq!(RealtimeAudio::pcm16_le([0, 1]).unwrap().samples(), 1);
        assert!(RealtimeAudio::pcm16_le([0]).is_err());
        assert_eq!(delegated_prompt(Some("plain request")), "plain request");
    }

    #[test]
    fn chunks_frameless_delegation_context_at_utf8_boundaries() {
        let text = format!("{}é{}", "a".repeat(499), "b".repeat(10));
        let chunks = context_append_chunks(&text);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 500));
        assert_eq!(context_append_chunks(""), [""]);
    }

    #[tokio::test]
    async fn streams_typed_audio_and_agent_results_over_one_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let update = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let update: serde_json::Value = serde_json::from_str(&update).unwrap();
            assert_eq!(update["type"], "session.update");
            socket
                .send(Message::Text(
                    r#"{"type":"session.updated","session":{"id":"rt_1"}}"#.into(),
                ))
                .await
                .unwrap();

            let audio = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let audio: serde_json::Value = serde_json::from_str(&audio).unwrap();
            assert_eq!(audio["type"], "input_audio_buffer.append");
            assert_eq!(audio["audio"], "AAE=");

            let output = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let output: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(output["item"]["call_id"], "call_1");
            assert_eq!(output["item"]["output"], "done");
            let create = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let create: serde_json::Value = serde_json::from_str(&create).unwrap();
            assert_eq!(create["type"], "response.create");
            socket
                .send(Message::Text(r#"{"type":"response.done"}"#.into()))
                .await
                .unwrap();

            let progress = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let progress: serde_json::Value = serde_json::from_str(&progress).unwrap();
            assert_eq!(progress["item"]["type"], "message");
            assert_eq!(progress["item"]["role"], "user");
            assert_eq!(progress["item"]["content"][0]["text"], "[BACKEND] working");

            let complete = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let complete: serde_json::Value = serde_json::from_str(&complete).unwrap();
            assert_eq!(complete["item"]["call_id"], "call_1");
            assert_eq!(
                complete["item"]["output"],
                "Background agent finished. Use the preceding [BACKEND] messages as the result."
            );
            let create = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let create: serde_json::Value = serde_json::from_str(&create).unwrap();
            assert_eq!(create["type"], "response.create");
            socket
                .send(Message::Text(r#"{"type":"response.done"}"#.into()))
                .await
                .unwrap();

            let steer = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let steer: serde_json::Value = serde_json::from_str(&steer).unwrap();
            assert_eq!(steer["item"]["call_id"], "call_2");
            assert_eq!(
                steer["item"]["output"],
                "This was sent to steer the previous background agent task."
            );
            let create = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let create: serde_json::Value = serde_json::from_str(&create).unwrap();
            assert_eq!(create["type"], "response.create");
        });

        let openai = OpenAi::new("test-key").unwrap();
        let (session, mut events) = openai
            .realtime("delegate coding work")
            .websocket_url(format!("ws://{address}"))
            .connect()
            .await
            .unwrap();
        assert_eq!(
            events.recv().await,
            Some(RealtimeEvent::SessionReady {
                session_id: "rt_1".to_owned(),
            })
        );
        session
            .send_audio(RealtimeAudio::pcm16_le([0, 1]).unwrap())
            .await
            .unwrap();
        session
            .complete_agent_request("call_1", "done")
            .await
            .unwrap();
        assert_eq!(events.recv().await, Some(RealtimeEvent::ResponseDone));
        session
            .append_agent_output("call_1", "working")
            .await
            .unwrap();
        session.complete_agent_run("call_1").await.unwrap();
        assert_eq!(events.recv().await, Some(RealtimeEvent::ResponseDone));
        assert_eq!(
            session.steer_agent_request("call_2").await.unwrap(),
            RealtimeAgentSteer::Acknowledged
        );
        server.await.unwrap();
    }
}
