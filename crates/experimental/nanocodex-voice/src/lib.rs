#![doc = include_str!("../README.md")]

use std::{io, sync::Arc, time::Duration};

use nanocodex::{
    Nanocodex, OpenAi,
    oai::{
        auth::OpenAiAuthMode,
        realtime::{RealtimeError, RealtimeEvent, RealtimeSession},
    },
};
use tokio::sync::{mpsc, oneshot};

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod audio;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[path = "audio_unsupported.rs"]
mod audio;

pub use nanocodex::oai::realtime::{
    CHATGPT_REALTIME_VOICE, CHATGPT_REALTIME_VOICES, PLATFORM_REALTIME_VOICE,
    PLATFORM_REALTIME_VOICES, RealtimeVoice,
};

use audio::VoiceAudio;

/// Default developer instructions for the conversational coding-agent bridge.
pub const DEFAULT_VOICE_INSTRUCTIONS: &str = r"You are the voice interface for a coding agent.
Be concise and conversational. Use background_agent whenever the user asks for repository work,
code changes, investigation, commands, or a factual answer that depends on the active coding
session. Preserve the user's own request in the tool argument. When its result arrives, summarize
the result accurately for speech. Do not claim work happened unless background_agent returned it.";

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
            instructions: Arc::from(DEFAULT_VOICE_INSTRUCTIONS),
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
        let (stop, stopped) = oneshot::channel();
        let task = std::thread::Builder::new()
            .name("nanocodex-voice".to_owned())
            .spawn(move || run_thread(self, events, stopped))
            .map_err(VoiceError::Spawn)?;
        Ok((
            VoiceSession {
                stop: Some(stop),
                task,
            },
            VoiceEvents { receiver },
        ))
    }
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
    let terminal = match runtime.block_on(run_voice(builder, &events, stopped)) {
        Ok(()) => VoiceEvent::Stopped,
        Err(error) => VoiceEvent::Failed { error },
    };
    send_event(&events, terminal);
}

async fn run_voice(
    builder: VoiceSessionBuilder,
    events: &mpsc::UnboundedSender<VoiceEvent>,
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
    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();
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
                    &builder.agent,
                    events,
                    &completed_tx,
                ).await {
                    break Err(error);
                }
            }
            completed = completed_rx.recv() => {
                let Some(CompletedAgentRequest { call_id, output }) = completed else {
                    break Ok(());
                };
                if let Err(error) = session.complete_agent_request(call_id, output).await {
                    break Err(error.into());
                }
            }
        }
    };
    drop(session.close().await);
    result
}

struct CompletedAgentRequest {
    call_id: String,
    output: String,
}

async fn handle_realtime_event(
    event: RealtimeEvent,
    session: &RealtimeSession,
    audio: &mut VoiceAudio,
    agent: &Nanocodex,
    events: &mpsc::UnboundedSender<VoiceEvent>,
    completed: &mpsc::UnboundedSender<CompletedAgentRequest>,
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
        RealtimeEvent::AgentRequest { call_id, prompt } => {
            let agent = agent.clone();
            let completed = completed.clone();
            drop(tokio::spawn(async move {
                let output = match agent.prompt(prompt).await {
                    Ok(turn) => match turn.await {
                        Ok(result) => result.final_message().to_owned(),
                        Err(error) => format!("The coding agent failed: {error}"),
                    },
                    Err(error) => format!("The coding agent rejected the request: {error}"),
                };
                drop(completed.send(CompletedAgentRequest { call_id, output }));
            }));
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

fn send_transcript(
    events: &mpsc::UnboundedSender<VoiceEvent>,
    speaker: VoiceSpeaker,
    text: String,
) {
    if !text.trim().is_empty() {
        send_event(events, VoiceEvent::Transcript { speaker, text });
    }
}

fn send_event(events: &mpsc::UnboundedSender<VoiceEvent>, event: VoiceEvent) {
    drop(events.send(event));
}

#[cfg(test)]
mod tests {
    use super::{AudioConfig, VoiceSpeaker};
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
}
