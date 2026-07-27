use std::{error::Error, io, path::PathBuf};

pub use nanocodex_oai_api::ResponsesError;
use nanocodex_oai_api::ResponsesServiceError;

/// Error returned by the Nanocodex library boundary.
#[derive(Debug, thiserror::Error)]
pub enum NanocodexError {
    /// Caller input or two configured policies are incompatible.
    #[error("invalid task request: {0}")]
    InvalidRequest(String),

    /// The configured workspace could not be resolved.
    #[error("failed to resolve task workspace {path}: {source}")]
    ResolveWorkspace {
        /// Workspace path supplied by the caller.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },

    /// The resolved workspace exists but is not a directory.
    #[error("task workspace is not a directory: {path}")]
    WorkspaceNotDirectory {
        /// Resolved workspace path.
        path: PathBuf,
    },

    /// The resolved workspace cannot be represented as UTF-8.
    #[error("task workspace path is not valid UTF-8: {path}")]
    WorkspaceNotUtf8 {
        /// Resolved workspace path.
        path: PathBuf,
    },

    /// A follow-on prompt attempted to change an owned session's workspace.
    #[error("an active agent session cannot change workspace from {current} to {requested}")]
    WorkspaceChanged {
        /// Workspace already owned by the session.
        current: String,
        /// Conflicting workspace requested by the caller.
        requested: String,
    },

    /// Project instructions could not be loaded from disk.
    #[error("failed to read project instructions from {path}: {source}")]
    ReadProjectInstructions {
        /// Instruction file that could not be read.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },

    /// A completed provider response violated an agent-loop invariant.
    #[error("malformed Responses API event: {detail}")]
    MalformedResponse {
        /// Stable invariant failure description.
        detail: &'static str,
    },

    /// A service returned an output for the wrong kind of attempt.
    #[error("invalid Responses attempt state: {detail}")]
    InvalidAttemptState {
        /// Stable invalid-state description.
        detail: &'static str,
    },

    /// The immutable request prefix could not be serialized for fingerprinting.
    #[error("failed to fingerprint the immutable prompt prefix: {0}")]
    SerializePromptPrefix(#[source] serde_json::Error),

    /// The private driver stopped before accepting a command.
    #[error("the agent stopped before accepting the command")]
    AgentStopped,

    /// The private driver stopped after accepting a turn but before delivering its result.
    #[error("the agent stopped before the turn completed")]
    TurnStopped,

    /// Steering targeted a queued or terminal turn.
    #[error("the targeted turn is queued, completed, or otherwise not active for steering")]
    TurnNotSteerable,

    /// The active turn cannot accept more queued steering input.
    #[error("the active turn's steering queue is full")]
    SteerQueueFull,

    /// Cancellation targeted an already terminal turn.
    #[error("the targeted turn has already completed or been cancelled")]
    TurnNotCancellable,

    /// The targeted turn was cancelled after its resources stopped.
    #[error("the turn was cancelled")]
    TurnCancelled,

    /// A fork was requested before any safe committed boundary existed.
    #[error("the agent has no safe conversation boundary to fork")]
    ForkBeforeCompletedTurn,

    /// A historical result came from a different conversation lineage.
    #[error("the completed turn belongs to a different conversation lineage")]
    CheckpointLineageMismatch,

    /// A serialized session snapshot failed structural or policy validation.
    #[error("invalid session snapshot: {0}")]
    InvalidSessionSnapshot(String),

    /// Agent construction was attempted outside an active Tokio runtime.
    #[error("building an agent requires an active Tokio runtime")]
    TokioRuntimeUnavailable,

    /// Codex-compatible rollout recording could not be initialized.
    #[error("failed to initialize a Codex rollout under {codex_home}: {source}")]
    InitializeRollout {
        /// Codex state directory selected by the caller.
        codex_home: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },

    /// A committed rollout could not be durably persisted.
    #[error("failed to persist Codex rollout at {path}: {source}")]
    PersistRollout {
        /// Rollout file that could not be written.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },

    /// Contractual agent event serialization failed.
    #[error(transparent)]
    Event(#[from] nanocodex_oai_api::EventError),

    /// The provider protocol or transport failed.
    #[error(transparent)]
    Responses(#[from] ResponsesError),

    /// The Tower Responses service failed.
    #[error(transparent)]
    ResponsesService(#[from] nanocodex_oai_api::ResponsesServiceError),

    /// The configured tool registry or runtime could not be built.
    #[cfg(not(target_family = "wasm"))]
    #[error("failed to build tools for an agent driver: {0}")]
    Tools(#[from] nanocodex_tools::ToolsBuildError),

    /// Caller-composed Tower middleware failed outside the typed service error.
    #[error("Responses service middleware failed: {0}")]
    ResponsesMiddleware(#[from] tower::BoxError),
}

impl NanocodexError {
    /// Returns the underlying Responses transport/API error, including when a
    /// caller-provided Tower middleware boxed the standard service error.
    #[must_use]
    pub fn responses_error(&self) -> Option<&ResponsesError> {
        match self {
            Self::Responses(error) => return Some(error),
            Self::ResponsesService(error) => return error.responses_error(),
            _ => {}
        }

        let mut current = self.source();
        while let Some(error) = current {
            if let Some(service) = error.downcast_ref::<ResponsesServiceError>() {
                return service.responses_error();
            }
            if let Some(responses) = error.downcast_ref::<ResponsesError>() {
                return Some(responses);
            }
            current = error.source();
        }
        None
    }
}

/// Result type returned by the owned agent lifecycle.
pub type Result<T> = std::result::Result<T, NanocodexError>;

#[cfg(test)]
mod tests {
    use super::{NanocodexError, ResponsesError};
    use nanocodex_oai_api::ResponsesServiceError;

    #[test]
    fn responses_classification_covers_every_service_boundary() {
        let direct = NanocodexError::Responses(ResponsesError::UnexpectedEnd);
        assert!(matches!(
            direct.responses_error(),
            Some(ResponsesError::UnexpectedEnd)
        ));

        let service = NanocodexError::ResponsesService(ResponsesServiceError::from(
            ResponsesError::UnexpectedEnd,
        ));
        assert!(matches!(
            service.responses_error(),
            Some(ResponsesError::UnexpectedEnd)
        ));

        let service = ResponsesServiceError::from(ResponsesError::UnexpectedEnd);
        let error = NanocodexError::ResponsesMiddleware(Box::new(service));
        assert!(matches!(
            error.responses_error(),
            Some(ResponsesError::UnexpectedEnd)
        ));
        assert_eq!(
            error.to_string(),
            "Responses service middleware failed: Responses WebSocket closed without a close frame"
        );
    }
}
