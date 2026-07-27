use std::{collections::HashMap, time::Duration};

use serde::Deserialize;

use tokio_tungstenite::tungstenite::{
    Error as WebSocketError, error::ProtocolError, http::header::InvalidHeaderValue,
};

/// Errors produced by the `OpenAI` Responses WebSocket transport.
#[derive(Debug, thiserror::Error)]
pub enum ResponsesError {
    /// Authorization could not be resolved.
    #[error("failed to resolve OpenAI authorization: {detail}")]
    Authorization {
        /// Credential-resolution detail without the credential value.
        detail: String,
    },
    /// The configured WebSocket URL was invalid.
    #[error("invalid Responses WebSocket URL")]
    InvalidUrl(#[source] WebSocketError),
    /// The authorization value could not be encoded as an HTTP header.
    #[error("invalid OpenAI authorization header")]
    InvalidAuthorization(#[source] InvalidHeaderValue),
    /// The session identity could not be encoded as an HTTP header.
    #[error("invalid Responses session identifier header")]
    InvalidSessionId(#[source] InvalidHeaderValue),
    /// The WebSocket handshake exceeded its deadline.
    #[error("Responses WebSocket handshake exceeded {seconds} seconds")]
    HandshakeTimeout {
        /// Configured timeout in seconds.
        seconds: u64,
    },
    /// The WebSocket handshake failed at the transport layer.
    #[error("Responses WebSocket handshake failed")]
    Handshake(#[source] WebSocketError),
    /// The server rejected the WebSocket handshake.
    #[error("Responses WebSocket handshake was rejected with HTTP {status}: {body}")]
    HandshakeRejected {
        /// HTTP response status.
        status: u16,
        /// Retained response body.
        body: String,
        /// Server-requested retry delay when present.
        retry_after: Option<Duration>,
    },
    /// Sending a WebSocket frame failed.
    #[error("failed to send a Responses WebSocket frame")]
    Send(#[source] WebSocketError),
    /// Sending a WebSocket frame exceeded its deadline.
    #[error("sending a Responses WebSocket frame exceeded {seconds} seconds")]
    SendTimeout {
        /// Configured timeout in seconds.
        seconds: u64,
    },
    /// No response event arrived before the idle deadline.
    #[error("Responses WebSocket produced no event for {seconds} seconds")]
    IdleTimeout {
        /// Configured idle timeout in seconds.
        seconds: u64,
    },
    /// The WebSocket stream ended without a close frame.
    #[error("Responses WebSocket closed without a close frame")]
    UnexpectedEnd,
    /// Receiving a WebSocket frame failed.
    #[error("failed to receive a Responses WebSocket frame")]
    Receive(#[source] WebSocketError),
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
    /// Sending or reading an HTTPS request failed.
    #[error("Responses HTTPS request failed")]
    HttpRequest(#[source] reqwest::Error),
    /// The server rejected an HTTPS request.
    #[error("Responses HTTPS request was rejected with HTTP {status}: {body}")]
    HttpRejected {
        /// HTTP response status.
        status: u16,
        /// Retained response body.
        body: String,
        /// Server-requested retry delay when present.
        retry_after: Option<Duration>,
    },
    /// An SSE response body contained invalid UTF-8.
    #[error("Responses HTTPS stream contained invalid UTF-8")]
    InvalidSseUtf8(#[source] std::str::Utf8Error),
}

impl ResponsesError {
    /// Returns the SDK-owned retry classification, if retrying is safe.
    #[must_use]
    pub fn retry_advice(&self) -> Option<RetryAdvice> {
        let (class, server_delay) = match self {
            Self::HandshakeTimeout { .. } => ("handshake_timeout", None),
            Self::Handshake(error) if is_transient_websocket(error) => {
                ("handshake_transport", None)
            }
            Self::HandshakeRejected {
                status,
                retry_after,
                ..
            } if *status == 429 => ("handshake_rate_limit", *retry_after),
            Self::HandshakeRejected {
                status,
                retry_after,
                ..
            } if (500..=599).contains(status) => ("handshake_server", *retry_after),
            Self::SendTimeout { .. } => ("send_timeout", None),
            Self::Send(error) if is_transient_websocket(error) => ("send_transport", None),
            Self::IdleTimeout { .. } => ("event_idle_timeout", None),
            Self::UnexpectedEnd | Self::Closed { .. } => ("premature_close", None),
            Self::Receive(error) if is_transient_websocket(error) => ("receive_transport", None),
            Self::Api { event } => retryable_api_error(event)?,
            Self::HttpRequest(error) if error.is_timeout() => ("https_timeout", None),
            Self::HttpRequest(error) if error.is_connect() || error.is_body() => {
                ("https_transport", None)
            }
            Self::HttpRejected {
                status,
                retry_after,
                ..
            } if *status == 429 => ("https_rate_limit", *retry_after),
            Self::HttpRejected {
                status,
                retry_after,
                ..
            } if (500..=599).contains(status) => ("https_server", *retry_after),
            _ => return None,
        };
        Some(RetryAdvice {
            class,
            server_delay,
        })
    }

    /// Returns a stable low-cardinality error class for telemetry.
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            Self::Authorization { .. } => "authorization",
            Self::InvalidUrl(_) => "invalid_url",
            Self::InvalidAuthorization(_) => "invalid_authorization",
            Self::InvalidSessionId(_) => "invalid_session_id",
            Self::HandshakeTimeout { .. } => "handshake_timeout",
            Self::Handshake(_) => "handshake",
            Self::HandshakeRejected { .. } => "handshake_rejected",
            Self::Send(_) => "send",
            Self::SendTimeout { .. } => "send_timeout",
            Self::IdleTimeout { .. } => "event_idle_timeout",
            Self::UnexpectedEnd => "premature_close",
            Self::Receive(_) => "receive",
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
            Self::HttpRequest(error) if error.is_timeout() => "https_timeout",
            Self::HttpRequest(_) => "https_transport",
            Self::HttpRejected { status: 429, .. } => "https_rate_limit",
            Self::HttpRejected { status, .. } if (500..=599).contains(status) => "https_server",
            Self::HttpRejected { .. } => "https_rejected",
            Self::InvalidSseUtf8(_) => "invalid_sse_utf8",
        }
    }

    /// Returns whether the provider no longer recognizes a continuation ID.
    #[must_use]
    pub fn is_checkpoint_missing(&self) -> bool {
        matches!(self, Self::Api { event } if api_error_has_code(event, "previous_response_not_found"))
    }

    /// Returns whether the provider rejected the request for context exhaustion.
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

/// Retry metadata derived from one typed transport or API error.
#[derive(Clone, Copy, Debug)]
pub struct RetryAdvice {
    /// Stable low-cardinality retry class.
    pub class: &'static str,
    /// Server-supplied minimum delay, if any.
    pub server_delay: Option<Duration>,
}

const fn is_transient_websocket(error: &WebSocketError) -> bool {
    matches!(
        error,
        WebSocketError::ConnectionClosed
            | WebSocketError::AlreadyClosed
            | WebSocketError::Io(_)
            | WebSocketError::Protocol(
                ProtocolError::HandshakeIncomplete
                    | ProtocolError::ResetWithoutClosingHandshake
                    | ProtocolError::SendAfterClosing
            )
    )
}

fn retryable_api_error(event: &str) -> Option<(&'static str, Option<Duration>)> {
    let event: ApiErrorEnvelope = serde_json::from_str(event).ok()?;
    let error = event
        .error
        .as_ref()
        .or_else(|| event.response.as_ref()?.error.as_ref())?;
    let class = match error.code.as_deref().or(error.kind.as_deref()) {
        Some(
            "server_is_overloaded"
            | "slow_down"
            | "server_error"
            | "websocket_connection_limit_reached",
        ) => "api_server",
        Some("rate_limit_exceeded") => "api_rate_limit",
        _ => return None,
    };
    let server_delay = error
        .retry_after
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .or_else(|| retry_after_header(&event.headers));
    Some((class, server_delay))
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

fn retry_after_header(headers: &HashMap<String, RetryAfterValue>) -> Option<Duration> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.seconds())
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    #[serde(default)]
    error: Option<ApiErrorDetail>,
    #[serde(default)]
    response: Option<ApiErrorResponse>,
    #[serde(default)]
    headers: HashMap<String, RetryAfterValue>,
}

#[derive(Deserialize)]
struct ApiErrorResponse {
    #[serde(default)]
    error: Option<ApiErrorDetail>,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    #[serde(default, rename = "type")]
    kind: Option<Box<str>>,
    #[serde(default)]
    code: Option<Box<str>>,
    #[serde(default)]
    retry_after: Option<f64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RetryAfterValue {
    Number(f64),
    String(Box<str>),
}

impl RetryAfterValue {
    fn seconds(&self) -> Option<f64> {
        match self {
            Self::Number(seconds) => Some(*seconds),
            Self::String(seconds) => seconds.parse().ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ResponsesError, retryable_api_error};

    #[test]
    fn retries_server_error_reported_as_error_type() {
        let event = r#"{
            "type":"error",
            "error":{
                "type":"server_error",
                "code":null,
                "message":"An error occurred while processing the request."
            }
        }"#;

        assert_eq!(
            retryable_api_error(event).map(|(class, _)| class),
            Some("api_server")
        );
    }

    #[test]
    fn classifies_context_window_failures_from_nested_response_errors() {
        let error = ResponsesError::api_event(
            r#"{
                "type": "response.failed",
                "response": {
                    "error": {
                        "code": "context_length_exceeded",
                        "message": "maximum context length exceeded"
                    }
                }
            }"#
            .to_owned(),
        );

        assert!(error.is_context_window_exceeded());
        assert_eq!(error.class(), "context_window_exceeded");
        assert!(error.retry_advice().is_none());
    }

    #[test]
    fn error_code_takes_precedence_over_error_type() {
        let event = r#"{
            "type":"error",
            "error":{
                "type":"server_error",
                "code":"invalid_prompt"
            }
        }"#;

        assert!(retryable_api_error(event).is_none());
    }
}
