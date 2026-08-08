//! Experimental synchronous continuation runtime backed by `codex-spine-core`.

use std::sync::{Arc, Mutex};

use codex_spine_core::{
    NodeId, NodeKind, NodeStatus, RawBoundary, RolloutEvent, SpineProjection, SpineReducer,
    ToolCallGroup, ToolOutcome, ToolUse,
};
use nanocodex::TerminalToolReceipt;
use serde::{Deserialize, Serialize};
use serde_json::json;

mod tools;

pub use tools::with_spine_tools;

const MAX_CONTINUATION_CONTEXT_TOKENS: usize = 1_000;
const APPROX_BYTES_PER_TOKEN: usize = 4;
const MAX_CONTINUATION_CONTEXT_BYTES: usize =
    MAX_CONTINUATION_CONTEXT_TOKENS * APPROX_BYTES_PER_TOKEN;

/// A receiver for reducer-derived tree snapshots suitable for a live UI.
pub type SpineTreeObserver = Arc<dyn Fn(SpineTreeSnapshot) + Send + Sync>;

/// A presentation-safe snapshot of the logical Spine tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpineTreeSnapshot {
    /// Node selected as the active continuation scope.
    pub active_node_id: String,
    /// Nodes in reducer source order.
    pub nodes: Vec<SpineTreeNode>,
}

/// One displayable logical Spine node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpineTreeNode {
    /// Stable reducer node id.
    pub id: String,
    /// Parent node id, if this is not a root epoch.
    pub parent_id: Option<String>,
    /// Whether this is the synthetic root or a model-owned task scope.
    pub kind: SpineTreeNodeKind,
    /// Current lifecycle state.
    pub status: SpineTreeNodeStatus,
    /// Model-authored scope summary, if applicable.
    pub summary: Option<String>,
}

/// The display-relevant class of a Spine node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpineTreeNodeKind {
    /// The synthetic root epoch for one user session.
    RootEpoch,
    /// A model-owned semantic task scope.
    Task,
}

/// The display-relevant lifecycle state of a Spine node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpineTreeNodeStatus {
    /// The active continuation scope.
    Live,
    /// A task scope that contains a still-live descendant.
    Opened,
    /// A completed task with compact memory in its parent context.
    Closed,
    /// A historical task elided by later compaction.
    Compacted,
}

/// Hard bounds for one synchronous continuation tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpineRuntimeLimits {
    /// Maximum number of live task scopes below the root epoch.
    pub max_depth: u32,
    /// Maximum number of task scopes accepted during the root session.
    pub max_nodes: u32,
    /// Maximum UTF-8 byte length of one child-scope summary, subject to the
    /// runtime's non-overridable context budget.
    pub max_summary_bytes: usize,
    /// Maximum UTF-8 byte length of one model-visible compact handoff, subject
    /// to the runtime's non-overridable context budget.
    pub max_memory_bytes: usize,
}

impl Default for SpineRuntimeLimits {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_nodes: 128,
            max_summary_bytes: 4 * 1024,
            max_memory_bytes: MAX_CONTINUATION_CONTEXT_BYTES,
        }
    }
}

/// Result of closing a continuation scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpineHandoff {
    /// The task node whose compact memory was committed.
    pub closed_node: NodeId,
    /// The task or parent node that is live after the transition.
    pub live_node: NodeId,
}

/// A committed terminal control transition for the continuation owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpineTerminalTransition {
    /// The current child ended and control returns to its frozen parent.
    Closed {
        /// Logical reducer state after the transition.
        handoff: SpineHandoff,
        /// Model-authored compact state returned to the parent tool call.
        memory: String,
    },
    /// The current child ended and its parent must start a sibling continuation.
    Next {
        /// Logical reducer state after the transition.
        handoff: SpineHandoff,
        /// The sibling task's concise goal.
        summary: String,
        /// Model-authored compact state from the closed sibling.
        memory: String,
    },
}

/// Validation errors from the continuation state machine.
#[derive(Debug, thiserror::Error)]
pub enum SpineRuntimeError {
    /// The model tried to open an unnamed scope.
    #[error("spine.open requires a non-empty summary")]
    EmptySummary,
    /// The model tried to finish a scope without a compact handoff.
    #[error("spine transition requires non-empty memory")]
    EmptyMemory,
    /// The child-scope summary exceeds the injected-context bound.
    #[error("spine summary exceeds the {0}-byte limit")]
    SummaryTooLarge(usize),
    /// The compact handoff exceeds the injected-context bound.
    #[error("spine memory exceeds the {0}-byte limit")]
    MemoryTooLarge(usize),
    /// The configured nesting cap would be exceeded.
    #[error("spine continuation depth limit of {0} reached")]
    DepthLimit(u32),
    /// The configured task-node cap would be exceeded.
    #[error("spine continuation node limit of {0} reached")]
    NodeLimit(u32),
    /// A close or next transition had no live task scope to finish.
    #[error("spine transition requires a live task scope")]
    NoLiveTask,
    /// An internal state mutex was poisoned by a previous panic.
    #[error("spine continuation state is unavailable")]
    StateUnavailable,
    /// The terminal receipt had no valid Spine control payload.
    #[error("spine terminal receipt has no valid control metadata")]
    InvalidTerminalReceipt,
    /// The terminal receipt name did not agree with its control payload.
    #[error("spine terminal receipt does not match its tool name")]
    ReceiptToolMismatch,
    /// A synthetic successful control event could not be encoded for the reducer.
    #[error("spine control event could not be encoded")]
    ControlEncoding,
}

/// Thread-safe event adapter around the canonical Spine reducer.
///
/// This type owns only logical Spine state. The application runtime owns the
/// parent/child Nanocodex sessions that execute each continuation scope.
pub struct SpineRuntime {
    limits: SpineRuntimeLimits,
    state: Mutex<RuntimeState>,
    tree_observer: Mutex<Option<SpineTreeObserver>>,
}

#[derive(Clone)]
struct RuntimeState {
    reducer: SpineReducer,
    next_boundary: u64,
    task_nodes: u32,
}

pub(crate) struct SpineRuntimeCheckpoint(RuntimeState);

impl SpineRuntime {
    /// Starts a fresh logical Spine root epoch.
    #[must_use]
    pub fn new(limits: SpineRuntimeLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(RuntimeState {
                reducer: SpineReducer::new(),
                next_boundary: 1,
                task_nodes: 0,
            }),
            tree_observer: Mutex::new(None),
        }
    }

    /// Commits a successful `spine.open` tool call and returns its child node.
    pub fn open(&self, call_id: &str, summary: &str) -> Result<NodeId, SpineRuntimeError> {
        self.validate_summary(summary)?;
        let summary = summary.trim();
        let (node, tree) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SpineRuntimeError::StateUnavailable)?;
            let projection = state.reducer.projection();
            let depth = projection.cursor.parts().len().saturating_sub(1);
            if depth >= self.limits.max_depth as usize {
                return Err(SpineRuntimeError::DepthLimit(self.limits.max_depth));
            }
            if state.task_nodes >= self.limits.max_nodes {
                return Err(SpineRuntimeError::NodeLimit(self.limits.max_nodes));
            }

            apply_control(
                &mut state,
                call_id,
                "spine.open",
                json!({ "summary": summary }),
            )?;
            state.task_nodes = state.task_nodes.saturating_add(1);
            let projection = state.reducer.projection();
            (projection.cursor.clone(), spine_tree_snapshot(&projection))
        };
        self.publish_tree(tree)?;
        Ok(node)
    }

    /// Commits a successful `spine.close` terminal receipt.
    pub fn close(&self, call_id: &str, memory: &str) -> Result<SpineHandoff, SpineRuntimeError> {
        self.validate_memory(memory)?;
        let memory = memory.trim();
        let (handoff, tree) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SpineRuntimeError::StateUnavailable)?;
            let closed_node = live_task(&state.reducer.projection())?;
            apply_control(
                &mut state,
                call_id,
                "spine.close",
                json!({ "memory": memory }),
            )?;
            let projection = state.reducer.projection();
            (
                SpineHandoff {
                    closed_node,
                    live_node: projection.cursor.clone(),
                },
                spine_tree_snapshot(&projection),
            )
        };
        self.publish_tree(tree)?;
        Ok(handoff)
    }

    /// Commits a successful `spine.next` terminal receipt.
    pub fn next(
        &self,
        call_id: &str,
        summary: &str,
        memory: &str,
    ) -> Result<SpineHandoff, SpineRuntimeError> {
        self.validate_summary(summary)?;
        self.validate_memory(memory)?;
        let summary = summary.trim();
        let memory = memory.trim();
        let (handoff, tree) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SpineRuntimeError::StateUnavailable)?;
            let closed_node = live_task(&state.reducer.projection())?;
            if state.task_nodes >= self.limits.max_nodes {
                return Err(SpineRuntimeError::NodeLimit(self.limits.max_nodes));
            }

            apply_control(
                &mut state,
                call_id,
                "spine.next",
                json!({ "summary": summary, "memory": memory }),
            )?;
            state.task_nodes = state.task_nodes.saturating_add(1);
            let projection = state.reducer.projection();
            (
                SpineHandoff {
                    closed_node,
                    live_node: projection.cursor.clone(),
                },
                spine_tree_snapshot(&projection),
            )
        };
        self.publish_tree(tree)?;
        Ok(handoff)
    }

    /// Validates and commits a terminal tool receipt after its enclosing Code
    /// Mode cell has succeeded.
    pub fn accept_terminal_receipt(
        &self,
        receipt: &TerminalToolReceipt,
    ) -> Result<SpineTerminalTransition, SpineRuntimeError> {
        let metadata = receipt
            .metadata()
            .ok_or(SpineRuntimeError::InvalidTerminalReceipt)?;
        let control = serde_json::from_str::<TerminalControl>(metadata.get())
            .map_err(|_| SpineRuntimeError::InvalidTerminalReceipt)?;
        match (receipt.tool_name(), control) {
            ("spine__close", TerminalControl::Close { memory }) => {
                let handoff = self.close(receipt.call_id(), &memory)?;
                Ok(SpineTerminalTransition::Closed { handoff, memory })
            }
            ("spine__next", TerminalControl::Next { summary, memory }) => {
                let handoff = self.next(receipt.call_id(), &summary, &memory)?;
                Ok(SpineTerminalTransition::Next {
                    handoff,
                    summary,
                    memory,
                })
            }
            _ => Err(SpineRuntimeError::ReceiptToolMismatch),
        }
    }

    /// Returns the current reducer-owned logical tree and visible context.
    pub fn projection(&self) -> Result<SpineProjection, SpineRuntimeError> {
        self.state
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)
            .map(|state| state.reducer.projection())
    }

    /// Replaces the live UI observer and immediately sends it the current tree.
    pub fn set_tree_observer(&self, observer: SpineTreeObserver) -> Result<(), SpineRuntimeError> {
        let tree = self.tree_snapshot()?;
        let mut stored = self
            .tree_observer
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)?;
        *stored = Some(Arc::clone(&observer));
        drop(stored);
        observer(tree);
        Ok(())
    }

    pub(crate) fn checkpoint(&self) -> Result<SpineRuntimeCheckpoint, SpineRuntimeError> {
        self.state
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)
            .map(|state| SpineRuntimeCheckpoint(state.clone()))
    }

    pub(crate) fn restore(
        &self,
        checkpoint: SpineRuntimeCheckpoint,
    ) -> Result<(), SpineRuntimeError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)?;
        *state = checkpoint.0;
        let tree = spine_tree_snapshot(&state.reducer.projection());
        drop(state);
        self.publish_tree(tree)?;
        Ok(())
    }

    pub(crate) fn ensure_live_task(&self) -> Result<(), SpineRuntimeError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)?;
        live_task(&state.reducer.projection()).map(|_| ())
    }

    pub(crate) fn validate_summary(&self, summary: &str) -> Result<(), SpineRuntimeError> {
        let summary = required(summary, SpineRuntimeError::EmptySummary)?;
        let limit = self.context_byte_limit(self.limits.max_summary_bytes);
        bounded(summary, limit, SpineRuntimeError::SummaryTooLarge(limit))
    }

    pub(crate) fn validate_memory(&self, memory: &str) -> Result<(), SpineRuntimeError> {
        let memory = required(memory, SpineRuntimeError::EmptyMemory)?;
        let limit = self.context_byte_limit(self.limits.max_memory_bytes);
        bounded(memory, limit, SpineRuntimeError::MemoryTooLarge(limit))
    }

    fn context_byte_limit(&self, configured_limit: usize) -> usize {
        configured_limit.min(MAX_CONTINUATION_CONTEXT_BYTES)
    }

    fn tree_snapshot(&self) -> Result<SpineTreeSnapshot, SpineRuntimeError> {
        self.state
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)
            .map(|state| spine_tree_snapshot(&state.reducer.projection()))
    }

    fn publish_tree(&self, tree: SpineTreeSnapshot) -> Result<(), SpineRuntimeError> {
        let observer = self
            .tree_observer
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)?
            .clone();
        if let Some(observer) = observer {
            observer(tree);
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TerminalControl {
    Close { memory: String },
    Next { summary: String, memory: String },
}

fn required(value: &str, error: SpineRuntimeError) -> Result<&str, SpineRuntimeError> {
    (!value.trim().is_empty()).then_some(value).ok_or(error)
}

fn bounded(value: &str, maximum: usize, error: SpineRuntimeError) -> Result<(), SpineRuntimeError> {
    (value.len() <= maximum).then_some(()).ok_or(error)
}

fn live_task(projection: &SpineProjection) -> Result<NodeId, SpineRuntimeError> {
    projection
        .nodes
        .iter()
        .find(|node| node.id == projection.cursor && node.kind == NodeKind::Task)
        .map(|node| node.id.clone())
        .ok_or(SpineRuntimeError::NoLiveTask)
}

fn spine_tree_snapshot(projection: &SpineProjection) -> SpineTreeSnapshot {
    SpineTreeSnapshot {
        active_node_id: projection.cursor.to_string(),
        nodes: projection
            .nodes
            .iter()
            .map(|node| SpineTreeNode {
                id: node.id.to_string(),
                parent_id: node.parent.as_ref().map(ToString::to_string),
                kind: match node.kind {
                    NodeKind::RootEpoch => SpineTreeNodeKind::RootEpoch,
                    NodeKind::Task => SpineTreeNodeKind::Task,
                },
                status: match node.status {
                    NodeStatus::Live => SpineTreeNodeStatus::Live,
                    NodeStatus::Opened => SpineTreeNodeStatus::Opened,
                    NodeStatus::Closed => SpineTreeNodeStatus::Closed,
                    NodeStatus::Compacted => SpineTreeNodeStatus::Compacted,
                },
                summary: node.summary.clone(),
            })
            .collect(),
    }
}

fn apply_control(
    state: &mut RuntimeState,
    call_id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> Result<(), SpineRuntimeError> {
    let arguments =
        serde_json::to_string(&arguments).map_err(|_| SpineRuntimeError::ControlEncoding)?;
    let start = RawBoundary(state.next_boundary);
    state.next_boundary = state.next_boundary.saturating_add(1);
    let end = RawBoundary(state.next_boundary);
    state.next_boundary = state.next_boundary.saturating_add(1);
    state.reducer.apply(RolloutEvent::ToolCall(ToolCallGroup {
        start,
        end,
        leading_assistant_messages: Vec::new(),
        calls: vec![ToolUse {
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            arguments,
            call_ordinal: None,
            outcome: Some(ToolOutcome::Succeeded),
            output: Some(r#"{"accepted":true}"#.to_owned()),
            output_boundary: Some(end),
        }],
    }));
    Ok(())
}
