use std::{
    io::Write,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue, to_raw_value};
use tokio::sync::mpsc;
use web_time::Instant;

const PROTOCOL_VERSION: u32 = 1;
static PROCESS_MONOTONIC_EPOCH: OnceLock<Instant> = OnceLock::new();

/// Returns a process-relative monotonic timestamp for private cross-layer timing.
#[doc(hidden)]
#[must_use]
pub fn monotonic_now_ns() -> u64 {
    let elapsed = PROCESS_MONOTONIC_EPOCH
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos();
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("failed to encode agent event")]
    Encode(#[source] serde_json::Error),

    #[error("failed to write agent event")]
    Write(#[source] std::io::Error),

    #[error("agent event stream closed before the turn emitted a terminal event")]
    ClosedBeforeTerminal,
}

/// One ordered event emitted by an agent run.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentEvent {
    pub protocol_version: u32,
    pub request_id: Arc<str>,
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: AgentEventKind,
    pub payload: Box<RawValue>,
}

/// Private in-process timing carried beside an event without changing JSONL.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct AgentEventTiming {
    pub emitted_ns: u64,
    pub source_received_ns: Option<u64>,
}

/// An agent event plus private in-process delivery timing.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct TimedAgentEvent {
    pub event: AgentEvent,
    pub timing: AgentEventTiming,
}

/// Stable event categories emitted by the agent runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AgentEventKind {
    #[serde(rename = "api.event")]
    ApiEvent,
    #[serde(rename = "assistant.delta")]
    AssistantDelta,
    #[serde(rename = "assistant.message")]
    AssistantMessage,
    #[serde(rename = "reasoning.summary.delta")]
    ReasoningSummaryDelta,
    #[serde(rename = "run.started")]
    RunStarted,
    #[serde(rename = "run.steered")]
    RunSteered,
    #[serde(rename = "run.error")]
    RunError,
    #[serde(rename = "run.completed")]
    RunCompleted,
    #[serde(rename = "run.failed")]
    RunFailed,
    #[serde(rename = "tool.call")]
    ToolCall,
    #[serde(rename = "tool.result")]
    ToolResult,
    #[serde(rename = "model.warmup.started")]
    ModelWarmupStarted,
    #[serde(rename = "model.warmup.completed")]
    ModelWarmupCompleted,
    #[serde(rename = "model.warmup.failed")]
    ModelWarmupFailed,
    #[serde(rename = "model.call.started")]
    ModelCallStarted,
    #[serde(rename = "model.call.completed")]
    ModelCallCompleted,
    #[serde(rename = "model.call.failed")]
    ModelCallFailed,
    #[serde(rename = "model.compaction.started")]
    ModelCompactionStarted,
    #[serde(rename = "model.compaction.completed")]
    ModelCompactionCompleted,
    #[serde(rename = "model.compaction.failed")]
    ModelCompactionFailed,
    #[serde(rename = "model.attempt.started")]
    ModelAttemptStarted,
    #[serde(rename = "model.attempt.failed")]
    ModelAttemptFailed,
    #[serde(rename = "model.attempt.retrying")]
    ModelAttemptRetrying,
    #[serde(rename = "model.connection.started")]
    ModelConnectionStarted,
    #[serde(rename = "model.connection.completed")]
    ModelConnectionCompleted,
    #[serde(rename = "model.connection.failed")]
    ModelConnectionFailed,
}

/// The receiving half of an agent's typed event stream.
pub struct AgentEvents {
    request_id: Arc<str>,
    receiver: mpsc::UnboundedReceiver<TimedAgentEvent>,
}

impl AgentEvents {
    /// Stable session/request identifier shared by every event in this stream.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Receives the next event, or `None` after all emitters are dropped.
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        self.recv_timed().await.map(|event| event.event)
    }

    /// Receives one event with private process-relative timing metadata.
    #[doc(hidden)]
    pub async fn recv_timed(&mut self) -> Option<TimedAgentEvent> {
        self.receiver.recv().await
    }

    /// Receives one immediately available event without waiting.
    #[doc(hidden)]
    pub fn try_recv_timed(&mut self) -> Option<TimedAgentEvent> {
        self.receiver.try_recv().ok()
    }

    /// Writes every event as one flushed JSONL record.
    ///
    /// # Errors
    ///
    /// Returns an error when an event cannot be encoded or written.
    pub async fn write_jsonl(mut self, mut output: impl Write) -> Result<(), EventError> {
        while let Some(event) = self.recv().await {
            write_jsonl_event(&mut output, &event)?;
        }
        Ok(())
    }

    /// Writes one turn through its terminal event and leaves the session stream
    /// available for follow-on turns.
    ///
    /// # Errors
    ///
    /// Returns an error when an event cannot be written or the agent stops
    /// before emitting `run.completed` or `run.failed`.
    pub async fn write_turn_jsonl(&mut self, mut output: impl Write) -> Result<(), EventError> {
        while let Some(event) = self.recv().await {
            let terminal = event.kind.is_terminal();
            write_jsonl_event(&mut output, &event)?;
            if terminal {
                return Ok(());
            }
        }
        Err(EventError::ClosedBeforeTerminal)
    }
}

impl Stream for AgentEvents {
    type Item = AgentEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.receiver
            .poll_recv(context)
            .map(|event| event.map(|event| event.event))
    }
}

impl AgentEventKind {
    /// Returns whether this event completes a turn.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::RunCompleted | Self::RunFailed)
    }
}

impl AgentEvent {
    /// Decodes the event payload into a caller-selected typed shape.
    ///
    /// # Errors
    ///
    /// Returns an error when the retained payload does not match `T`.
    pub fn decode_payload<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(self.payload.get())
    }
}

fn write_jsonl_event(output: &mut impl Write, event: &AgentEvent) -> Result<(), EventError> {
    serde_json::to_writer(&mut *output, event).map_err(EventError::Encode)?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(EventError::Write)
}

/// Internal emission handle shared by orchestration and transport crates.
#[doc(hidden)]
#[derive(Clone)]
pub struct EventSink {
    request_id: Arc<str>,
    next_seq: Arc<AtomicU64>,
    sender: mpsc::UnboundedSender<TimedAgentEvent>,
}

impl EventSink {
    #[must_use]
    pub fn channel(request_id: String) -> (Self, AgentEvents) {
        let request_id = Arc::<str>::from(request_id);
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                request_id: Arc::clone(&request_id),
                next_seq: Arc::new(AtomicU64::new(1)),
                sender,
            },
            AgentEvents {
                request_id,
                receiver,
            },
        )
    }

    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Emits an event when a receiver is present and otherwise discards it.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload cannot be converted to JSON.
    pub fn emit<P: Serialize>(&self, kind: AgentEventKind, payload: P) -> Result<(), EventError> {
        self.emit_with_sequence(kind, payload).map(|_| ())
    }

    /// Emits an event and returns its session-monotonic sequence number.
    ///
    /// This is intended for transport telemetry that must correlate the point
    /// of emission with a downstream consumer without retaining payload data.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload cannot be converted to JSON.
    #[doc(hidden)]
    pub fn emit_with_sequence<P: Serialize>(
        &self,
        kind: AgentEventKind,
        payload: P,
    ) -> Result<u64, EventError> {
        self.emit_with_source_sequence(kind, payload, None)
    }

    /// Emits an event correlated with the process-monotonic source receipt time.
    #[doc(hidden)]
    pub fn emit_with_source_sequence<P: Serialize>(
        &self,
        kind: AgentEventKind,
        payload: P,
        source_received_ns: Option<u64>,
    ) -> Result<u64, EventError> {
        if self.sender.is_closed() {
            return Ok(self.next_seq.fetch_add(1, Ordering::Relaxed));
        }
        let payload = to_raw_value(&payload).map_err(EventError::Encode)?;
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        drop(self.sender.send(TimedAgentEvent {
            event: AgentEvent {
                protocol_version: PROTOCOL_VERSION,
                request_id: Arc::clone(&self.request_id),
                seq,
                kind,
                payload,
            },
            timing: AgentEventTiming {
                emitted_ns: monotonic_now_ns(),
                source_received_ns,
            },
        }));
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Serialize, Serializer};
    use serde_json::json;

    use super::{AgentEventKind, EventSink};

    #[test]
    fn events_are_ordered_and_receiver_drop_is_not_an_error() {
        let (events, mut receiver) = EventSink::channel("request-1".to_owned());
        assert_eq!(receiver.request_id(), "request-1");
        events
            .emit(AgentEventKind::RunStarted, json!({ "n": 1 }))
            .unwrap();
        events
            .emit(AgentEventKind::RunCompleted, json!({ "n": 2 }))
            .unwrap();
        let first = receiver.receiver.try_recv().unwrap().event;
        let second = receiver.receiver.try_recv().unwrap().event;
        assert_eq!((first.seq, first.kind), (1, AgentEventKind::RunStarted));
        assert_eq!((second.seq, second.kind), (2, AgentEventKind::RunCompleted));
        assert_eq!(
            second.decode_payload::<serde_json::Value>().unwrap()["n"],
            2
        );
        drop(receiver);
        events.emit(AgentEventKind::RunFailed, json!({})).unwrap();
    }

    #[test]
    fn receiver_drop_skips_payload_serialization() {
        struct MustNotSerialize;

        impl Serialize for MustNotSerialize {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                panic!("closed event streams must not serialize payloads")
            }
        }

        let (events, receiver) = EventSink::channel("request-1".to_owned());
        drop(receiver);

        assert_eq!(
            events
                .emit_with_sequence(AgentEventKind::ApiEvent, MustNotSerialize)
                .unwrap(),
            1
        );
    }

    #[test]
    fn timing_is_private_and_preserves_the_jsonl_contract() {
        let (events, mut receiver) = EventSink::channel("request-1".to_owned());
        let source_received_ns = super::monotonic_now_ns();
        events
            .emit_with_source_sequence(
                AgentEventKind::AssistantDelta,
                json!({ "text": "x" }),
                Some(source_received_ns),
            )
            .unwrap();

        let timed = receiver.receiver.try_recv().unwrap();
        assert_eq!(timed.timing.source_received_ns, Some(source_received_ns));
        assert!(timed.timing.emitted_ns >= source_received_ns);
        let encoded = serde_json::to_value(&timed.event).unwrap();
        assert!(encoded.get("timing").is_none());
        assert!(encoded.get("source_received_ns").is_none());
        assert_eq!(encoded["type"], "assistant.delta");
    }

    #[test]
    fn timed_events_can_be_drained_without_async_receive_round_trips() {
        let (events, mut receiver) = EventSink::channel("request-1".to_owned());
        for n in 1..=3 {
            events
                .emit(AgentEventKind::AssistantDelta, json!({ "n": n }))
                .unwrap();
        }

        let sequences = std::iter::from_fn(|| receiver.try_recv_timed())
            .map(|event| event.event.seq)
            .collect::<Vec<_>>();
        assert_eq!(sequences, vec![1, 2, 3]);
        assert!(receiver.try_recv_timed().is_none());
    }
}
