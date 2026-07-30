# nanocodex-voice

`nanocodex-voice` is the experimental reusable desktop consumer for
Nanocodex's device-neutral GPT Realtime API. It connects the default microphone
and speaker, delegates repository work to an existing retained `Nanocodex`
agent, and exposes lifecycle and transcript updates as typed events.

Realtime handoffs use the agent's atomic live-input router. The first coding
request starts an independently awaitable turn; follow-up speech received while
that turn is running is admitted to its bounded steering queue and joins at the
next safe model boundary, including after an in-flight tool result. Realtime V2
acknowledges the steering tool call immediately; Frameless retargets the open
delegation to the newest request. Neither path waits behind the active turn as
a second queued request.

If another UI can start work through the same agent before voice does, mirror
its session-wide `AgentEvent`s through `VoiceSession::observe_agent_event`.
That lets the voice handoff attach to the already-running turn, stream its
remaining output, and announce completion. The Nanocodex TUI wires this path
automatically.

The default conversational prompt, local first-name rendering, delegation
markers, tool descriptions, and protocol-specific steering acknowledgement are
ported from Codex's Realtime implementation. Callers can still replace the
conversation prompt with `VoiceSessionBuilder::instructions`, or reuse the
rendered default and delegation envelope independently through
`codex_voice_instructions()` and `codex_realtime_delegation()`.

```rust,no_run
use nanocodex::{Nanocodex, OpenAi};
use nanocodex_voice::{VoiceEvent, VoiceSessionBuilder};

# async fn example(openai: OpenAi, agent: Nanocodex) -> Result<(), Box<dyn std::error::Error>> {
let (mut voice, mut events) = VoiceSessionBuilder::new(openai, agent).spawn()?;
while let Some(event) = events.recv().await {
    match event {
        VoiceEvent::Transcript { speaker, text } => println!("{speaker}: {text}"),
        VoiceEvent::Failed { error } => return Err(error.into()),
        VoiceEvent::Stopped => break,
        VoiceEvent::Connecting | VoiceEvent::Started { .. } => {}
    }
}
voice.stop();
# Ok(())
# }
```

The lower `nanocodex-oai-api::realtime` module remains the transport contract
for custom devices, pipes, and non-desktop embeddings. This crate deliberately
packages one opinionated native lifecycle rather than moving audio-device
policy into the public OpenAI boundary. The Nanocodex Ratatui `/voice` command
is a thin consumer of this crate.

Default-device capture and playback are currently implemented on macOS and
Windows. Other native targets return a typed unsupported-platform failure and
can continue to use the raw PCM Realtime API.
