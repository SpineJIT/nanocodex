use nanocodex::{
    Nanocodex, OpenAi,
    oai::{
        auth::{OpenAiAuth, load_chatgpt_auth},
        realtime::{RealtimeAudio, RealtimeEvent},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

const VOICE_INSTRUCTIONS: &str = r"You are a concise voice interface for a coding agent.
Use background_agent for repository work and preserve the user's own request in its prompt.
Summarize the completed background result accurately for speech.";

struct CompletedRequest {
    call_id: String,
    output: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let openai = OpenAi::new(load_auth()?)?;
    let (agent, _) = Nanocodex::builder(openai.clone())
        .instructions("Act as a coding agent. Inspect before claiming and preserve unrelated work.")
        .workspace(std::env::current_dir()?)
        .build()?;
    let mut realtime = openai.realtime(VOICE_INSTRUCTIONS);
    if let Ok(attestation) = std::env::var("NANOCODEX_REALTIME_ATTESTATION") {
        realtime = realtime.attestation_header(attestation);
    }
    let (realtime, mut events) = realtime.connect().await?;

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut input = [0_u8; 1_920];
    let mut trailing_byte = None;
    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();

    loop {
        tokio::select! {
            read = stdin.read(&mut input) => {
                let read = read?;
                if read == 0 {
                    break;
                }
                let mut pcm = Vec::with_capacity(read + usize::from(trailing_byte.is_some()));
                if let Some(byte) = trailing_byte.take() {
                    pcm.push(byte);
                }
                pcm.extend_from_slice(&input[..read]);
                if pcm.len() % 2 != 0 {
                    trailing_byte = pcm.pop();
                }
                realtime.send_audio(RealtimeAudio::pcm16_le(pcm)?).await?;
            }
            event = events.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    RealtimeEvent::Audio(audio) => {
                        stdout.write_all(audio.as_bytes()).await?;
                        stdout.flush().await?;
                    }
                    RealtimeEvent::AgentRequest { call_id, prompt } => {
                        let agent = agent.clone();
                        let completed_tx = completed_tx.clone();
                        tokio::spawn(async move {
                            let output = match agent.prompt(prompt).await {
                                Ok(turn) => match turn.await {
                                    Ok(result) => result.final_message().to_owned(),
                                    Err(error) => format!("The coding agent failed: {error}"),
                                },
                                Err(error) => format!("The coding agent rejected the request: {error}"),
                            };
                            drop(completed_tx.send(CompletedRequest { call_id, output }));
                        });
                    }
                    RealtimeEvent::RemainSilent { call_id } => {
                        realtime.complete_silent_request(call_id).await?;
                    }
                    RealtimeEvent::Error(error) => return Err(error.into()),
                    RealtimeEvent::SessionReady { .. }
                    | RealtimeEvent::SpeechStarted
                    | RealtimeEvent::InputTranscriptDelta(_)
                    | RealtimeEvent::InputTranscriptDone(_)
                    | RealtimeEvent::OutputTranscriptDelta(_)
                    | RealtimeEvent::OutputTranscriptDone(_)
                    | RealtimeEvent::ResponseStarted
                    | RealtimeEvent::ResponseDone => {}
                }
            }
            completed = completed_rx.recv() => {
                let Some(CompletedRequest { call_id, output }) = completed else {
                    break;
                };
                realtime.complete_agent_request(call_id, output).await?;
            }
        }
    }

    realtime.close().await?;
    agent.shutdown().await?;
    Ok(())
}

fn load_auth() -> Result<OpenAiAuth, Box<dyn std::error::Error>> {
    if let Some(path) = std::env::var_os("NANOCODEX_AUTH_FILE") {
        return Ok(load_chatgpt_auth(path)?);
    }
    Ok(OpenAiAuth::api_key(std::env::var("OPENAI_API_KEY")?))
}
