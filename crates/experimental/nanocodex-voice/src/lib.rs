#![doc = include_str!("../README.md")]

use std::{io, sync::Arc, time::Duration};

use futures_util::StreamExt;
use nanocodex::{
    Nanocodex, OpenAi, PromptRoute,
    agent::events::{AgentEvent, AgentEventData, AssistantEvent, RunEvent},
    oai::{
        auth::OpenAiAuthMode,
        realtime::{
            RealtimeAgentSteer, RealtimeError, RealtimeEvent, RealtimeSession,
            RealtimeTranscriptEntry,
        },
    },
};
use tokio::sync::{mpsc, oneshot};

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod audio;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[allow(clippy::missing_const_for_fn)]
#[path = "audio_unsupported.rs"]
mod audio;

pub use nanocodex::oai::realtime::{
    CHATGPT_REALTIME_VOICE, CHATGPT_REALTIME_VOICES, PLATFORM_REALTIME_VOICE,
    PLATFORM_REALTIME_VOICES, RealtimeVoice,
};

use audio::VoiceAudio;

const CODEX_BACKEND_PROMPT: &str = include_str!("backend_prompt.md");
const USER_FIRST_NAME_PLACEHOLDER: &str = "{{ user_first_name }}";
const DEFAULT_USER_FIRST_NAME: &str = "there";
const HANDOFF_STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(200);
const REALTIME_ASSISTANT_OUTPUT_TOKEN_BUDGET: usize = 1_000;
const APPROX_BYTES_PER_TOKEN: usize = 4;
const HANDOFF_STREAM_TRUNCATION_MARKER: &str = "\n…output truncated…\n";

/// Desktop capture and playback policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioConfig {
    playback_prebuffer: Duration,
    maximum_playback_buffer: Duration,
}

impl AudioConfig {
    /// Creates a desktop audio policy with the requested playout buffering.
    #[must_use]
    pub const fn new(playback_prebuffer: Duration, maximum_playback_buffer: Duration) -> Self {
        Self {
            playback_prebuffer,
            maximum_playback_buffer,
        }
    }

    /// Returns the audio accumulated before playout begins or resumes.
    #[must_use]
    pub const fn playback_prebuffer(self) -> Duration {
        self.playback_prebuffer
    }

    /// Returns the maximum decoded audio retained for playout.
    #[must_use]
    pub const fn maximum_playback_buffer(self) -> Duration {
        self.maximum_playback_buffer
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self::new(Duration::from_millis(120), Duration::from_secs(8))
    }
}

/// The participant associated with a completed voice transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceSpeaker {
    /// The local microphone user.
    User,
    /// The Realtime voice assistant.
    Assistant,
}

impl std::fmt::Display for VoiceSpeaker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        })
    }
}

/// One typed update from an experimental desktop voice lifecycle.
#[derive(Debug)]
pub enum VoiceEvent {
    /// The Realtime transport is connecting.
    Connecting,
    /// The Realtime transport and default audio devices are active.
    Started {
        /// The selected output voice.
        voice: RealtimeVoice,
    },
    /// A participant's completed transcript.
    Transcript {
        /// The participant that produced the transcript.
        speaker: VoiceSpeaker,
        /// The complete transcript text.
        text: String,
    },
    /// The voice lifecycle failed and stopped.
    Failed {
        /// The terminal typed failure.
        error: VoiceFailure,
    },
    /// The voice lifecycle stopped cleanly.
    Stopped,
}

/// Receiver for one independent desktop voice event stream.
pub struct VoiceEvents {
    receiver: mpsc::UnboundedReceiver<VoiceEvent>,
}

impl VoiceEvents {
    /// Waits for the next lifecycle or transcript update.
    pub async fn recv(&mut self) -> Option<VoiceEvent> {
        self.receiver.recv().await
    }

    /// Attempts to receive an already-buffered update.
    pub fn try_recv(&mut self) -> Option<VoiceEvent> {
        self.receiver.try_recv().ok()
    }
}

/// A running desktop voice lifecycle.
pub struct VoiceSession {
    stop: Option<oneshot::Sender<()>>,
    agent_events: mpsc::UnboundedSender<AgentEvent>,
    task: std::thread::JoinHandle<()>,
}

impl VoiceSession {
    /// Returns whether the owned voice thread is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.task.is_finished()
    }

    /// Requests a clean stop without blocking the caller.
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    /// Mirrors one session-wide agent event into an active externally-started handoff.
    ///
    /// Embeddings whose typed UI can start a turn before voice is enabled should
    /// pass that agent's normal [`AgentEvent`] stream here. Events for turns
    /// started by this voice session are already mirrored internally.
    #[must_use]
    pub fn observe_agent_event(&self, event: AgentEvent) -> bool {
        self.agent_events.send(event).is_ok()
    }
}

impl Drop for VoiceSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Builder for one reusable desktop voice-to-agent lifecycle.
pub struct VoiceSessionBuilder {
    openai: OpenAi,
    agent: Nanocodex,
    instructions: Arc<str>,
    session_id: Option<Arc<str>>,
    attestation_header: Option<Arc<str>>,
    voice: Option<RealtimeVoice>,
    audio: AudioConfig,
}

impl VoiceSessionBuilder {
    /// Creates a voice lifecycle over an existing OpenAI recipe and agent.
    #[must_use]
    pub fn new(openai: OpenAi, agent: Nanocodex) -> Self {
        Self {
            openai,
            agent,
            instructions: Arc::from(codex_voice_instructions()),
            session_id: None,
            attestation_header: None,
            voice: None,
            audio: AudioConfig::default(),
        }
    }

    /// Replaces the voice model's developer instructions.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<Arc<str>>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// Supplies a stable caller-owned identity for transport correlation.
    #[must_use]
    pub fn session_id(mut self, session_id: impl Into<Arc<str>>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Supplies a host-generated ChatGPT device-attestation value.
    #[must_use]
    pub fn attestation_header(mut self, value: impl Into<Arc<str>>) -> Self {
        self.attestation_header = Some(value.into());
        self
    }

    /// Selects an explicit output voice.
    #[must_use]
    pub const fn voice(mut self, voice: RealtimeVoice) -> Self {
        self.voice = Some(voice);
        self
    }

    /// Replaces desktop capture and playback policy.
    #[must_use]
    pub const fn audio_config(mut self, audio: AudioConfig) -> Self {
        self.audio = audio;
        self
    }

    /// Spawns the owned desktop lifecycle and its independent event stream.
    ///
    /// # Errors
    ///
    /// Returns an error when the lifecycle thread cannot be created. Runtime,
    /// transport, and audio-device failures are delivered through
    /// [`VoiceEvent::Failed`].
    pub fn spawn(self) -> Result<(VoiceSession, VoiceEvents), VoiceError> {
        let (events, receiver) = mpsc::unbounded_channel();
        let (agent_events, observed_agent_events) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let task = std::thread::Builder::new()
            .name("nanocodex-voice".to_owned())
            .spawn(move || run_thread(self, events, observed_agent_events, stopped))
            .map_err(VoiceError::Spawn)?;
        Ok((
            VoiceSession {
                stop: Some(stop),
                agent_events,
                task,
            },
            VoiceEvents { receiver },
        ))
    }
}

/// Renders Codex's default Realtime backend prompt for the local user.
#[must_use]
pub fn codex_voice_instructions() -> String {
    CODEX_BACKEND_PROMPT
        .trim_end()
        .replace(USER_FIRST_NAME_PLACEHOLDER, &current_user_first_name())
}

fn current_user_first_name() -> String {
    [whoami::realname(), whoami::username()]
        .into_iter()
        .filter_map(|name| name.split_whitespace().next().map(str::to_owned))
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_USER_FIRST_NAME.to_owned())
}

/// Failure to create the owned desktop voice lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    /// The lifecycle thread could not be created.
    #[error("failed to spawn desktop voice thread: {0}")]
    Spawn(#[source] io::Error),
}

/// Terminal failure from an active desktop voice lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum VoiceFailure {
    /// The dedicated async runtime could not be initialized.
    #[error("failed to create voice runtime: {0}")]
    Runtime(String),
    /// The GPT Realtime transport failed.
    #[error(transparent)]
    Realtime(#[from] RealtimeError),
    /// The Realtime session reported a provider-side failure event.
    #[error("GPT Realtime reported an error: {0}")]
    Provider(String),
    /// Default-device capture or playback failed.
    #[error(transparent)]
    Audio(#[from] AudioError),
    /// The default microphone stream ended unexpectedly.
    #[error("microphone stream stopped")]
    MicrophoneStopped,
}

/// Failure to configure or operate native audio devices.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// No default microphone was available.
    #[error("no default microphone is available")]
    NoInputDevice,
    /// No default speaker was available.
    #[error("no default audio output is available")]
    NoOutputDevice,
    /// The requested audio policy was invalid.
    #[error("invalid desktop audio policy: {0}")]
    InvalidConfig(&'static str),
    /// The platform audio backend rejected an operation.
    #[error("{operation}: {message}")]
    Backend {
        /// The failed operation.
        operation: &'static str,
        /// Backend diagnostic text.
        message: String,
    },
    /// Default-device ownership is not implemented on this target.
    #[error(
        "default microphone/speaker capture is currently supported on macOS and Windows; use nanocodex-oai-api's PCM Realtime API with a platform adapter"
    )]
    UnsupportedPlatform,
}

fn run_thread(
    builder: VoiceSessionBuilder,
    events: mpsc::UnboundedSender<VoiceEvent>,
    observed_agent_events: mpsc::UnboundedReceiver<AgentEvent>,
    stopped: oneshot::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            send_event(
                &events,
                VoiceEvent::Failed {
                    error: VoiceFailure::Runtime(error.to_string()),
                },
            );
            return;
        }
    };
    let terminal =
        match runtime.block_on(run_voice(builder, &events, observed_agent_events, stopped)) {
            Ok(()) => VoiceEvent::Stopped,
            Err(error) => VoiceEvent::Failed { error },
        };
    send_event(&events, terminal);
}

async fn run_voice(
    builder: VoiceSessionBuilder,
    events: &mpsc::UnboundedSender<VoiceEvent>,
    mut observed_agent_events: mpsc::UnboundedReceiver<AgentEvent>,
    mut stopped: oneshot::Receiver<()>,
) -> Result<(), VoiceFailure> {
    send_event(events, VoiceEvent::Connecting);
    let voice = builder.voice.unwrap_or(match builder.openai.auth_mode() {
        OpenAiAuthMode::ChatGpt => CHATGPT_REALTIME_VOICE,
        OpenAiAuthMode::ApiKey => PLATFORM_REALTIME_VOICE,
    });
    let mut realtime = builder.openai.realtime(builder.instructions).voice(voice);
    if let Some(session_id) = builder.session_id {
        realtime = realtime.session_id(session_id.as_ref());
    }
    if let Some(attestation) = builder.attestation_header {
        realtime = realtime.attestation_header(attestation);
    }
    let connect = realtime.connect();
    let (session, mut realtime_events) = tokio::select! {
        result = connect => result?,
        _ = &mut stopped => return Ok(()),
    };
    let (mut audio, mut microphone) = VoiceAudio::open(builder.audio)?;
    let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel();
    let mut agent_bridge = AgentBridge {
        agent: builder.agent.clone(),
        updates: bridge_tx,
        active: None,
        next_generation: 0,
        external_output: HandoffStream::default(),
        external_error: None,
    };
    let mut external_flush = tokio::time::interval(HANDOFF_STREAM_FLUSH_INTERVAL);
    external_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    external_flush.tick().await;
    send_event(events, VoiceEvent::Started { voice });

    let result = loop {
        tokio::select! {
            _ = &mut stopped => break Ok(()),
            frame = microphone.recv() => {
                let Some(frame) = frame else {
                    break Err(VoiceFailure::MicrophoneStopped);
                };
                if let Err(error) = session.send_audio(frame).await {
                    break Err(error.into());
                }
            }
            event = realtime_events.recv() => {
                let Some(event) = event else {
                    break Ok(());
                };
                if let Err(error) = handle_realtime_event(
                    event,
                    &session,
                    &mut audio,
                    events,
                    &mut agent_bridge,
                ).await {
                    break Err(error);
                }
            }
            update = bridge_rx.recv() => {
                let Some(update) = update else {
                    break Ok(());
                };
                match update {
                    AgentBridgeUpdate::Output { generation, text } => {
                        if let Some(active) = &mut agent_bridge.active
                            && active.generation == generation
                        {
                            session.append_agent_output(&active.call_id, text).await?;
                            active.streamed_output = true;
                        }
                    }
                    AgentBridgeUpdate::Completed {
                        generation,
                        call_id,
                        output,
                    } => {
                        let (call_id, streamed_output) = match agent_bridge.active.take() {
                            Some(active) if active.generation == generation => {
                                (active.call_id, active.streamed_output)
                            }
                            Some(active) => {
                                agent_bridge.active = Some(active);
                                (call_id, false)
                            }
                            None => (call_id, false),
                        };
                        if !streamed_output && !output.trim().is_empty() {
                            session.append_agent_output(&call_id, output).await?;
                        }
                        session.complete_agent_run(call_id).await?;
                    }
                }
            }
            event = observed_agent_events.recv() => {
                let Some(event) = event else {
                    continue;
                };
                handle_observed_agent_event(event, &session, &mut agent_bridge).await?;
            }
            _ = external_flush.tick(), if agent_bridge.has_external_stream_output() => {
                flush_observed_agent_output(&session, &mut agent_bridge).await?;
            }
        }
    };
    drop(session.close().await);
    result
}

enum AgentBridgeUpdate {
    Output {
        generation: u64,
        text: String,
    },
    Completed {
        generation: u64,
        call_id: String,
        output: String,
    },
}

struct ActiveAgentRequest {
    generation: u64,
    call_id: String,
    streamed_output: bool,
    external: bool,
}

struct AgentBridge {
    agent: Nanocodex,
    updates: mpsc::UnboundedSender<AgentBridgeUpdate>,
    active: Option<ActiveAgentRequest>,
    next_generation: u64,
    external_output: HandoffStream,
    external_error: Option<String>,
}

impl AgentBridge {
    fn has_external_stream_output(&self) -> bool {
        self.active.as_ref().is_some_and(|active| active.external)
            && !self.external_output.is_empty()
    }
}

async fn handle_realtime_event(
    event: RealtimeEvent,
    session: &RealtimeSession,
    audio: &mut VoiceAudio,
    events: &mpsc::UnboundedSender<VoiceEvent>,
    agent_bridge: &mut AgentBridge,
) -> Result<(), VoiceFailure> {
    match event {
        RealtimeEvent::SessionReady { .. }
        | RealtimeEvent::InputTranscriptDelta(_)
        | RealtimeEvent::OutputTranscriptDelta(_)
        | RealtimeEvent::ResponseStarted
        | RealtimeEvent::ResponseDone => {}
        RealtimeEvent::SpeechStarted => audio.interrupt(),
        RealtimeEvent::InputTranscriptDone(text) => {
            send_transcript(events, VoiceSpeaker::User, text);
        }
        RealtimeEvent::OutputTranscriptDone(text) => {
            send_transcript(events, VoiceSpeaker::Assistant, text);
        }
        RealtimeEvent::Audio(frame) => audio.play(&frame),
        RealtimeEvent::AgentRequest {
            call_id,
            prompt,
            transcript,
        } => {
            let agent = agent_bridge.agent.clone();
            let updates = agent_bridge.updates.clone();
            let streams_agent_output = session.streams_agent_output();
            match agent
                .route_prompt(codex_realtime_delegation_with_transcript(
                    &prompt,
                    &transcript,
                ))
                .await
            {
                Ok(PromptRoute::Started(turn)) => {
                    agent_bridge.next_generation = agent_bridge.next_generation.saturating_add(1);
                    let generation = agent_bridge.next_generation;
                    agent_bridge.active = Some(ActiveAgentRequest {
                        generation,
                        call_id: call_id.clone(),
                        streamed_output: false,
                        external: false,
                    });
                    drop(tokio::spawn(async move {
                        let mut turn = turn;
                        let mut output = HandoffStream::default();
                        let mut flush = tokio::time::interval(HANDOFF_STREAM_FLUSH_INTERVAL);
                        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        flush.tick().await;
                        loop {
                            tokio::select! {
                                event = turn.next() => {
                                    let Some(event) = event else {
                                        break;
                                    };
                                    match event.data() {
                                        Ok(AgentEventData::Assistant(AssistantEvent::Delta(delta)))
                                            if streams_agent_output =>
                                        {
                                            output.push_text(&delta.text);
                                        }
                                        Ok(AgentEventData::Assistant(AssistantEvent::Message(message)))
                                            if streams_agent_output =>
                                        {
                                            if !output.has_output() {
                                                output.push_text(&message.text);
                                            }
                                            if let Some(text) = output.drain_final_chunk()
                                                && updates.send(AgentBridgeUpdate::Output {
                                                    generation,
                                                    text,
                                                }).is_err()
                                            {
                                                return;
                                            }
                                            output = HandoffStream::default();
                                        }
                                        Ok(AgentEventData::Assistant(AssistantEvent::Message(message)))
                                            if !streams_agent_output
                                                && !message.text.is_empty()
                                                && updates.send(AgentBridgeUpdate::Output {
                                                    generation,
                                                    text: truncate_realtime_output(&message.text),
                                                }).is_err() =>
                                        {
                                            return;
                                        }
                                        _ => {}
                                    }
                                }
                                _ = flush.tick(), if streams_agent_output && !output.is_empty() => {
                                    if let Some(text) = output.drain_stream_chunk()
                                        && updates.send(AgentBridgeUpdate::Output {
                                            generation,
                                            text,
                                        }).is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                        if let Some(text) = output.drain_final_chunk()
                            && updates
                                .send(AgentBridgeUpdate::Output { generation, text })
                                .is_err()
                        {
                            return;
                        }
                        let output = match turn.result().await {
                            Ok(result) => result.final_message().to_owned(),
                            Err(error) => format!("The coding agent failed: {error}"),
                        };
                        drop(updates.send(AgentBridgeUpdate::Completed {
                            generation,
                            call_id,
                            output,
                        }));
                    }));
                }
                Ok(PromptRoute::Steered) => {
                    if agent_bridge.active.is_none() {
                        agent_bridge.next_generation =
                            agent_bridge.next_generation.saturating_add(1);
                        agent_bridge.active = Some(ActiveAgentRequest {
                            generation: agent_bridge.next_generation,
                            call_id: call_id.clone(),
                            streamed_output: false,
                            external: true,
                        });
                        agent_bridge.external_output = HandoffStream::default();
                        agent_bridge.external_error = None;
                    }
                    if session.steer_agent_request(&call_id).await?
                        == RealtimeAgentSteer::ReplacedDelegation
                        && let Some(active) = &mut agent_bridge.active
                    {
                        active.call_id = call_id;
                    }
                }
                Err(error) => {
                    session
                        .append_agent_output(
                            &call_id,
                            format!("The coding agent rejected the request: {error}"),
                        )
                        .await?;
                    session.complete_agent_run(call_id).await?;
                }
            }
        }
        RealtimeEvent::RemainSilent { call_id } => {
            session.complete_silent_request(call_id).await?;
        }
        RealtimeEvent::Error(error) => {
            return Err(VoiceFailure::Provider(error));
        }
    }
    Ok(())
}

async fn handle_observed_agent_event(
    event: AgentEvent,
    session: &RealtimeSession,
    agent_bridge: &mut AgentBridge,
) -> Result<(), VoiceFailure> {
    if !agent_bridge
        .active
        .as_ref()
        .is_some_and(|active| active.external)
    {
        return Ok(());
    }

    match event.data() {
        Ok(AgentEventData::Assistant(AssistantEvent::Delta(delta)))
            if session.streams_agent_output() =>
        {
            agent_bridge.external_output.push_text(&delta.text);
        }
        Ok(AgentEventData::Assistant(AssistantEvent::Message(message))) => {
            let output = if session.streams_agent_output() {
                if !agent_bridge.external_output.has_output() {
                    agent_bridge.external_output.push_text(&message.text);
                }
                let output = agent_bridge.external_output.drain_final_chunk();
                agent_bridge.external_output = HandoffStream::default();
                output
            } else if message.text.is_empty() {
                None
            } else {
                Some(truncate_realtime_output(&message.text))
            };
            if let Some(output) = output {
                append_observed_agent_output(session, agent_bridge, output).await?;
            }
        }
        Ok(AgentEventData::Run(RunEvent::Error(error))) => {
            agent_bridge.external_error = Some(error.message);
        }
        Ok(AgentEventData::Run(RunEvent::Completed(_))) => {
            complete_observed_agent_run(session, agent_bridge, false).await?;
        }
        Ok(AgentEventData::Run(RunEvent::Failed(_))) => {
            complete_observed_agent_run(session, agent_bridge, true).await?;
        }
        Ok(_) | Err(_) => {}
    }
    Ok(())
}

async fn flush_observed_agent_output(
    session: &RealtimeSession,
    agent_bridge: &mut AgentBridge,
) -> Result<(), RealtimeError> {
    if let Some(output) = agent_bridge.external_output.drain_stream_chunk() {
        append_observed_agent_output(session, agent_bridge, output).await?;
    }
    Ok(())
}

async fn append_observed_agent_output(
    session: &RealtimeSession,
    agent_bridge: &mut AgentBridge,
    output: String,
) -> Result<(), RealtimeError> {
    let Some(call_id) = agent_bridge
        .active
        .as_ref()
        .filter(|active| active.external)
        .map(|active| active.call_id.clone())
    else {
        return Ok(());
    };
    session.append_agent_output(&call_id, output).await?;
    if let Some(active) = &mut agent_bridge.active
        && active.external
        && active.call_id == call_id
    {
        active.streamed_output = true;
    }
    Ok(())
}

async fn complete_observed_agent_run(
    session: &RealtimeSession,
    agent_bridge: &mut AgentBridge,
    failed: bool,
) -> Result<(), RealtimeError> {
    if let Some(output) = agent_bridge.external_output.drain_final_chunk() {
        append_observed_agent_output(session, agent_bridge, output).await?;
    }
    agent_bridge.external_output = HandoffStream::default();

    let Some(active) = agent_bridge.active.take().filter(|active| active.external) else {
        return Ok(());
    };
    if failed && !active.streamed_output {
        let error = agent_bridge
            .external_error
            .take()
            .unwrap_or_else(|| "The coding agent failed.".to_owned());
        session.append_agent_output(&active.call_id, error).await?;
    } else {
        agent_bridge.external_error = None;
    }
    session.complete_agent_run(active.call_id).await
}

#[derive(Default)]
struct HandoffStream {
    sent_bytes: usize,
    buffered_text: String,
    tail_text: String,
    truncated: bool,
}

impl HandoffStream {
    const fn has_output(&self) -> bool {
        self.sent_bytes > 0 || !self.is_empty()
    }

    const fn is_empty(&self) -> bool {
        self.buffered_text.is_empty() && self.tail_text.is_empty()
    }

    const fn stream_head_byte_limit(&self) -> usize {
        realtime_output_byte_limit().saturating_sub(HANDOFF_STREAM_TRUNCATION_MARKER.len()) / 2
    }

    const fn tail_byte_limit(&self) -> usize {
        realtime_output_byte_limit()
            .saturating_sub(self.stream_head_byte_limit())
            .saturating_sub(HANDOFF_STREAM_TRUNCATION_MARKER.len())
    }

    const fn streamable_text_bytes(&self) -> usize {
        self.stream_head_byte_limit()
            .saturating_sub(self.sent_bytes)
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.truncated {
            self.tail_text.push_str(text);
            self.tail_text = take_last_bytes(&self.tail_text, self.tail_byte_limit()).to_owned();
            return;
        }

        self.buffered_text.push_str(text);
        let remaining = realtime_output_byte_limit().saturating_sub(self.sent_bytes);
        if self.buffered_text.len() <= remaining {
            return;
        }

        let head_bytes = take_first_bytes(&self.buffered_text, self.streamable_text_bytes()).len();
        self.tail_text = take_last_bytes(&self.buffered_text, self.tail_byte_limit()).to_owned();
        self.buffered_text.truncate(head_bytes);
        self.truncated = true;
    }

    fn drain_stream_chunk(&mut self) -> Option<String> {
        let requested = self.streamable_text_bytes().min(self.buffered_text.len());
        let split_at = take_first_bytes(&self.buffered_text, requested).len();
        if split_at == 0 {
            return None;
        }
        let text = self.buffered_text.drain(..split_at).collect::<String>();
        self.sent_bytes = self.sent_bytes.saturating_add(text.len());
        Some(text)
    }

    fn drain_final_chunk(&mut self) -> Option<String> {
        if !self.truncated {
            if self.buffered_text.is_empty() {
                return None;
            }
            let text = self.buffered_text.drain(..).collect::<String>();
            self.sent_bytes = self.sent_bytes.saturating_add(text.len());
            return Some(text);
        }

        let head = self.buffered_text.drain(..).collect::<String>();
        let tail = self.tail_text.drain(..).collect::<String>();
        let text = format!("{head}{HANDOFF_STREAM_TRUNCATION_MARKER}{tail}");
        self.sent_bytes = self.sent_bytes.saturating_add(text.len());
        Some(text)
    }
}

const fn realtime_output_byte_limit() -> usize {
    REALTIME_ASSISTANT_OUTPUT_TOKEN_BUDGET.saturating_mul(APPROX_BYTES_PER_TOKEN)
}

fn truncate_realtime_output(text: &str) -> String {
    let mut output = HandoffStream::default();
    output.push_text(text);
    output.drain_final_chunk().unwrap_or_default()
}

fn take_first_bytes(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn take_last_bytes(text: &str, max_bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn send_transcript(
    events: &mpsc::UnboundedSender<VoiceEvent>,
    speaker: VoiceSpeaker,
    text: String,
) {
    if !text.trim().is_empty() {
        send_event(events, VoiceEvent::Transcript { speaker, text });
    }
}

/// Wraps delegated speech in Codex's model-visible Realtime input markers.
#[must_use]
pub fn codex_realtime_delegation(input: &str) -> String {
    codex_realtime_delegation_with_transcript(input, &[])
}

/// Wraps delegated speech and its new conversation transcript using Codex's markers.
#[must_use]
pub fn codex_realtime_delegation_with_transcript(
    input: &str,
    transcript: &[RealtimeTranscriptEntry],
) -> String {
    let input = input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let transcript = transcript
        .iter()
        .map(|entry| format!("{}: {}", entry.role, entry.text))
        .collect::<Vec<_>>()
        .join("\n")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if transcript.is_empty() {
        format!("<realtime_delegation>\n  <input>{input}</input>\n</realtime_delegation>")
    } else {
        format!(
            "<realtime_delegation>\n  <input>{input}</input>\n  <transcript_delta>{transcript}</transcript_delta>\n</realtime_delegation>"
        )
    }
}

fn send_event(events: &mpsc::UnboundedSender<VoiceEvent>, event: VoiceEvent) {
    drop(events.send(event));
}

#[cfg(test)]
mod tests {
    use super::{
        AudioConfig, HandoffStream, RealtimeTranscriptEntry, VoiceSpeaker,
        codex_realtime_delegation, codex_realtime_delegation_with_transcript,
        codex_voice_instructions, realtime_output_byte_limit, truncate_realtime_output,
    };
    use std::time::Duration;

    #[test]
    fn desktop_audio_policy_is_explicit_and_stable() {
        let config = AudioConfig::default();
        assert_eq!(config.playback_prebuffer(), Duration::from_millis(120));
        assert_eq!(config.maximum_playback_buffer(), Duration::from_secs(8));
    }

    #[test]
    fn transcript_speakers_have_stable_labels() {
        assert_eq!(VoiceSpeaker::User.to_string(), "user");
        assert_eq!(VoiceSpeaker::Assistant.to_string(), "assistant");
    }

    #[test]
    fn codex_backend_prompt_is_rendered_for_the_local_user() {
        let prompt = codex_voice_instructions();
        assert!(prompt.starts_with("## Identity, tone, and role"));
        assert!(prompt.contains("Running backend work remains steerable."));
        assert!(!prompt.contains("{{ user_first_name }}"));
    }

    #[test]
    fn delegated_input_uses_codex_markers_and_xml_escaping() {
        assert_eq!(
            codex_realtime_delegation("fix <x> & ship"),
            "<realtime_delegation>\n  <input>fix &lt;x&gt; &amp; ship</input>\n</realtime_delegation>"
        );
        assert_eq!(
            codex_realtime_delegation_with_transcript(
                "ship it",
                &[
                    RealtimeTranscriptEntry {
                        role: "assistant".to_owned(),
                        text: "Use <main>".to_owned(),
                    },
                    RealtimeTranscriptEntry {
                        role: "user".to_owned(),
                        text: "yes & now".to_owned(),
                    },
                ],
            ),
            "<realtime_delegation>\n  <input>ship it</input>\n  <transcript_delta>assistant: Use &lt;main&gt;\nuser: yes &amp; now</transcript_delta>\n</realtime_delegation>"
        );
    }

    #[test]
    fn codex_handoff_stream_is_bounded_and_preserves_head_and_tail() {
        let text = format!("HEAD{}TAIL", "x".repeat(8_000));
        let truncated = truncate_realtime_output(&text);
        assert!(truncated.len() <= realtime_output_byte_limit());
        assert!(truncated.starts_with("HEAD"));
        assert!(truncated.ends_with("TAIL"));
        assert!(truncated.contains("\n…output truncated…\n"));

        let mut stream = HandoffStream::default();
        let short = "é".repeat(1_500);
        stream.push_text(&short);
        let head = stream.drain_stream_chunk().unwrap();
        let tail = stream.drain_final_chunk().unwrap();
        assert_eq!(format!("{head}{tail}"), short);
    }
}
