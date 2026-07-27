use std::time::Duration;

/// Errors produced by the host-backed Responses WebSocket transport.
#[derive(Debug, thiserror::Error)]
pub enum ResponsesError {
    /// Authorization could not be resolved.
    #[error("failed to resolve OpenAI authorization: {detail}")]
    Authorization {
        /// Credential-resolution detail without the credential value.
        detail: String,
    },
    /// Establishing the host-backed WebSocket connection failed.
    #[error("failed to connect to the Responses WebSocket: {detail}")]
    Connect {
        /// Host transport failure detail.
        detail: String,
    },
    /// Sending a host-backed WebSocket frame failed.
    #[error("failed to send a Responses WebSocket frame: {detail}")]
    Send {
        /// Host transport failure detail.
        detail: String,
        /// Whether opening a replacement socket may safely recover.
        reconnectable: bool,
    },
    /// Receiving a host-backed WebSocket frame failed.
    #[error("failed to receive a Responses WebSocket frame: {0}")]
    Receive(String),
    /// No response event arrived before the idle deadline.
    #[error("Responses WebSocket produced no event for {seconds} seconds")]
    IdleTimeout {
        /// Configured idle timeout in seconds.
        seconds: u64,
    },
    /// The WebSocket stream ended without a close frame.
    #[error("Responses WebSocket closed without a close frame")]
    UnexpectedEnd,
    /// A received WebSocket event was not valid JSON.
    #[error("Responses WebSocket event was not valid JSON")]
    InvalidJson(#[source] serde_json::Error),
    /// The endpoint returned a binary frame where text JSON was required.
    #[error("Responses WebSocket returned a binary data frame; expected JSON text")]
    UnexpectedBinary,
    /// A typed request could not be serialized.
    #[error("failed to encode a Responses WebSocket request")]
    EncodeRequest(#[source] serde_json::Error),
    /// An event's payload did not match the shape declared by its type.
    #[error("Responses API event did not match its declared type: {event}")]
    InvalidPayload {
        /// Typed payload decode failure.
        #[source]
        source: serde_json::Error,
        /// Complete retained provider event.
        event: String,
    },
    /// The WebSocket closed with provider-supplied detail.
    #[error("Responses WebSocket closed {detail}")]
    Closed {
        /// Close code and reason.
        detail: String,
    },
    /// The Responses API returned a typed error event.
    #[error("Responses API returned an error event: {event}")]
    Api {
        /// Complete retained provider event.
        event: String,
    },
    /// The request exceeded the model context window.
    #[error("Responses input exceeded the model context window")]
    ContextWindowExceeded {
        /// Complete retained provider event.
        event: String,
    },
    /// The provider rejected malformed or unsupported image data.
    #[error("Responses API rejected invalid image data: {event}")]
    InvalidImageRequest {
        /// Complete retained provider event.
        event: String,
    },
}

impl ResponsesError {
    /// Returns the SDK-owned retry classification, if retrying is safe.
    #[must_use]
    pub fn retry_advice(&self) -> Option<RetryAdvice> {
        let class = match self {
            Self::Connect { .. } => "handshake_transport",
            Self::Send {
                reconnectable: true,
                ..
            } => "send_transport",
            Self::Receive(_) => "receive_transport",
            Self::IdleTimeout { .. } => "event_idle_timeout",
            Self::UnexpectedEnd | Self::Closed { .. } => "premature_close",
            Self::Authorization { .. }
            | Self::Send {
                reconnectable: false,
                ..
            }
            | Self::InvalidJson(_)
            | Self::UnexpectedBinary
            | Self::EncodeRequest(_)
            | Self::InvalidPayload { .. }
            | Self::Api { .. }
            | Self::ContextWindowExceeded { .. }
            | Self::InvalidImageRequest { .. } => return None,
        };
        Some(RetryAdvice {
            class,
            server_delay: None,
        })
    }

    /// Returns a stable low-cardinality error class for telemetry.
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            Self::Authorization { .. } => "authorization",
            Self::Connect { .. } => "handshake",
            Self::Send { .. } => "send",
            Self::Receive(_) => "receive",
            Self::IdleTimeout { .. } => "event_idle_timeout",
            Self::UnexpectedEnd => "premature_close",
            Self::InvalidJson(_) => "invalid_json",
            Self::UnexpectedBinary => "unexpected_binary",
            Self::EncodeRequest(_) => "encode_request",
            Self::InvalidPayload { .. } => "invalid_payload",
            Self::Closed { .. } => "closed",
            Self::Api { event } if api_error_has_code(event, "previous_response_not_found") => {
                "checkpoint_missing"
            }
            Self::Api { .. } => "api",
            Self::ContextWindowExceeded { .. } => "context_window_exceeded",
            Self::InvalidImageRequest { .. } => "invalid_image_request",
        }
    }

    /// Whether the provider no longer has the requested response checkpoint.
    #[must_use]
    pub fn is_checkpoint_missing(&self) -> bool {
        matches!(self, Self::Api { event } if api_error_has_code(event, "previous_response_not_found"))
    }

    /// Whether the provider rejected the request for exceeding its context window.
    #[must_use]
    pub const fn is_context_window_exceeded(&self) -> bool {
        matches!(self, Self::ContextWindowExceeded { .. })
    }

    pub(crate) fn api_event(event: String) -> Self {
        if api_error_has_code(&event, "context_length_exceeded") {
            Self::ContextWindowExceeded { event }
        } else {
            Self::Api { event }
        }
    }
}

/// Safe retry classification owned by the Responses transport.
#[derive(Clone, Copy, Debug)]
pub struct RetryAdvice {
    /// Stable low-cardinality retry class for telemetry.
    pub class: &'static str,
    /// Provider-requested delay before the next attempt, when supplied.
    pub server_delay: Option<Duration>,
}

fn api_error_has_code(event: &str, expected: &str) -> bool {
    let Ok(event) = serde_json::from_str::<ApiErrorEnvelope>(event) else {
        return false;
    };
    let code = event
        .error
        .as_ref()
        .or_else(|| {
            event
                .response
                .as_ref()
                .and_then(|response| response.error.as_ref())
        })
        .and_then(|error| error.code.as_deref());
    code == Some(expected)
}

#[derive(serde::Deserialize)]
struct ApiErrorEnvelope {
    #[serde(default)]
    error: Option<ApiErrorDetail>,
    #[serde(default)]
    response: Option<ApiErrorResponse>,
}

#[derive(serde::Deserialize)]
struct ApiErrorResponse {
    #[serde(default)]
    error: Option<ApiErrorDetail>,
}

#[derive(serde::Deserialize)]
struct ApiErrorDetail {
    #[serde(default)]
    code: Option<Box<str>>,
}
