# nanocodex-voice

`nanocodex-voice` is the experimental reusable desktop consumer for
Nanocodex's device-neutral GPT Realtime API. It connects the default microphone
and speaker, delegates repository work to an existing retained `Nanocodex`
agent, and exposes lifecycle and transcript updates as typed events.

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
