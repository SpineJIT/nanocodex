//! Experimental durable Spine continuation runtime backed by `codex-spine-core`.

use std::{
    collections::BTreeSet,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
};

use codex_spine_core::{NodeKind, NodeStatus, SpineProjection};
use nanocodex::TerminalToolReceipt;
use serde::{Deserialize, Serialize};

use crate::journal::{
    DeliveryKind, DeliveryStatus, Journal, JournalError, JournalHeader, TransitionIntent,
    TransitionKind,
};

mod journal;
mod tools;

#[cfg(test)]
#[path = "journal_tests.rs"]
mod journal_tests;

pub use tools::with_spine_tools;

const MAX_CONTINUATION_CONTEXT_BYTES: usize = 1_000;
const MAX_HANDOFF_MEMORY_BYTES: usize = 900;
const MAX_DELIVERY_ID_BYTES: usize = 64;

/// A receiver for reducer-derived tree snapshots suitable for a live UI.
pub type SpineTreeObserver = Arc<dyn Fn(SpineTreeSnapshot) + Send + Sync>;

/// A future returned by a coordinator-owned Spine intent capability.
pub type SpinePrepareFuture =
    Pin<Box<dyn Future<Output = Result<(), SpineRuntimeError>> + Send + 'static>>;

/// Weak model-tool capability that durably prepares one terminal Spine control.
///
/// The application coordinator implements this trait. Tools can request a
/// prepared transition but cannot fork, switch the active session, or mutate
/// the reducer themselves.
pub trait SpineIntentSink: Send + Sync {
    /// Appends and syncs a validated prepared intent before the tool succeeds.
    fn prepare(&self, request: SpineIntentRequest) -> SpinePrepareFuture;
}

/// One model-tool request to prepare a terminal Spine transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpineIntentRequest {
    source_session_id: String,
    terminal_call_id: String,
    control: SpineTerminalControl,
}

impl SpineIntentRequest {
    /// Creates one request from the executing session and nested tool call.
    #[must_use]
    pub fn new(
        source_session_id: impl Into<String>,
        terminal_call_id: impl Into<String>,
        control: SpineTerminalControl,
    ) -> Self {
        Self {
            source_session_id: source_session_id.into(),
            terminal_call_id: terminal_call_id.into(),
            control,
        }
    }
}

/// The bounded payload committed by a terminal Spine tool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpineTerminalControl {
    /// Park the active node and activate one child scope.
    Open {
        /// Concise child-scope goal.
        summary: String,
    },
    /// Close the active child and return compact state to its parent.
    Close {
        /// Compact continuation handoff.
        memory: String,
    },
    /// Close the active child and activate a sibling from its frozen parent.
    Next {
        /// Concise sibling-scope goal.
        summary: String,
        /// Compact state from the closed sibling.
        memory: String,
    },
}

/// The bounded cause recorded when a prepared transition cannot commit.
///
/// This is deliberately a closed set: journals are a durable protocol, so a
/// free-form diagnostic must not become replayed state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpineAbortReason {
    /// The coordinator stopped before it could commit the prepared transition.
    CoordinatorStoppedBeforeCommit,
    /// Creating or switching the target session failed before commit.
    TerminalTransitionFailed,
    /// A terminal tool receipt did not accompany the completed turn.
    MissingTerminalReceipt,
    /// The enclosing model turn was cancelled.
    TurnCancelled,
    /// The enclosing model turn failed.
    TurnFailed,
}

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
    /// The synthetic root epoch.
    RootEpoch,
    /// A model-owned task scope.
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

/// Hard bounds for one durable Spine tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpineRuntimeLimits {
    /// Maximum number of live task scopes below the root epoch.
    pub max_depth: u32,
    /// Maximum number of task scopes accepted during the root session.
    pub max_nodes: u32,
    /// Maximum UTF-8 byte length of one child-scope summary.
    pub max_summary_bytes: usize,
    /// Maximum UTF-8 byte length of one model-visible compact handoff.
    pub max_memory_bytes: usize,
}

impl Default for SpineRuntimeLimits {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_nodes: 128,
            max_summary_bytes: 4 * 1024,
            max_memory_bytes: MAX_HANDOFF_MEMORY_BYTES,
        }
    }
}

/// One journal-prepared or journal-committed semantic transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpineTransition {
    intent: TransitionIntent,
}

impl SpineTransition {
    /// Returns the control kind selected by the model.
    #[must_use]
    pub const fn kind(&self) -> SpineTransitionKind {
        match self.intent.kind() {
            TransitionKind::Open => SpineTransitionKind::Open,
            TransitionKind::Close => SpineTransitionKind::Close,
            TransitionKind::Next => SpineTransitionKind::Next,
        }
    }

    /// Returns the source session that issued the terminal tool call.
    #[must_use]
    pub fn source_session_id(&self) -> &str {
        self.intent.source_session_id()
    }

    /// Returns the nested terminal tool call ID.
    #[must_use]
    pub fn terminal_call_id(&self) -> &str {
        self.intent.terminal_call_id()
    }

    /// Returns the frozen parent session for close or next.
    #[must_use]
    pub fn parent_session_id(&self) -> Option<&str> {
        self.intent.parent_session_id()
    }
}

/// The semantic operation selected by one terminal control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpineTransitionKind {
    /// Park the source and activate a durable child.
    Open,
    /// Close the source and reactivate its parent.
    Close,
    /// Close the source and activate a sibling forked from its parent.
    Next,
}

/// A durable at-most-once continuation or recovery delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpineDelivery {
    id: String,
    target_session_id: String,
    kind: SpineDeliveryKind,
    transition: SpineTransition,
}

impl SpineDelivery {
    /// Returns the durable delivery identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the only session allowed to receive this delivery.
    #[must_use]
    pub fn target_session_id(&self) -> &str {
        &self.target_session_id
    }

    /// Returns the delivery's semantic purpose.
    #[must_use]
    pub const fn kind(&self) -> SpineDeliveryKind {
        self.kind
    }

    /// Returns the transition whose context this delivery carries.
    #[must_use]
    pub const fn transition(&self) -> &SpineTransition {
        &self.transition
    }
}

/// Whether a delivery advances a committed transition or repairs an aborted one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpineDeliveryKind {
    /// Continue at the target selected by a committed transition.
    Continuation,
    /// Explain an aborted prepared transition and ask the source to continue.
    Recovery,
}

/// Durable progress for one continuation or recovery prompt delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpineDeliveryStatus {
    /// The target has not yet been asked to accept the prompt.
    Unclaimed,
    /// The prompt may have been submitted but its acceptance was not synced.
    Claimed,
    /// Nano accepted the prompt command and its future is now ordinary work.
    Accepted,
}

/// Validation and durability errors from the Spine runtime.
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
    /// The full model-visible continuation context exceeds the hard cap.
    #[error("spine continuation context exceeds the {0}-byte limit")]
    ContinuationContextTooLarge(usize),
    /// The configured nesting cap would be exceeded.
    #[error("spine continuation depth limit of {0} reached")]
    DepthLimit(u32),
    /// The configured task-node cap would be exceeded.
    #[error("spine continuation node limit of {0} reached")]
    NodeLimit(u32),
    /// A close or next transition had no live task scope to finish.
    #[error("spine transition requires a live task scope")]
    NoLiveTask,
    /// No prepared operation corresponds to the terminal receipt.
    #[error("spine terminal receipt has no prepared transition")]
    MissingPreparedTransition,
    /// The terminal receipt differs from its prepared control.
    #[error("spine terminal receipt does not match its prepared control")]
    PreparedTransitionMismatch,
    /// The terminal receipt had no valid Spine control payload.
    #[error("spine terminal receipt has no valid control metadata")]
    InvalidTerminalReceipt,
    /// The terminal receipt name did not agree with its control payload.
    #[error("spine terminal receipt does not match its tool name")]
    ReceiptToolMismatch,
    /// A parked session attempted to control the active continuation stack.
    #[error("spine transition must originate from the active session")]
    InactiveSession,
    /// The journal owner stopped before an intent could be prepared.
    #[error("spine coordinator is no longer available")]
    CoordinatorStopped,
    /// The coordinator is durably switching the active node.
    #[error("spine continuation transition is in progress")]
    TransitionInProgress,
    /// A runtime mutex was poisoned by a previous panic.
    #[error("spine continuation state is unavailable")]
    StateUnavailable,
    /// An application-private journal operation failed.
    #[error("spine journal failed: {0}")]
    Journal(String),
}

/// Durable logical state for one Spine root session.
///
/// The runtime owns only the sidecar journal, reducer projection, validation,
/// and UI observation. The binary-level coordinator owns Nano sessions and
/// routes commands to exactly one active session.
pub struct SpineRuntime {
    limits: SpineRuntimeLimits,
    journal: Mutex<Journal>,
    tree_observer: Mutex<Option<SpineTreeObserver>>,
}

impl SpineRuntime {
    /// Creates and synchronizes a fresh sidecar journal for one root session.
    pub fn create(
        limits: SpineRuntimeLimits,
        directory: &Path,
        root_session_id: impl Into<String>,
        prompt_cache_key: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Result<Self, SpineRuntimeError> {
        let header = JournalHeader::new(root_session_id, prompt_cache_key, created_at);
        let journal = Journal::create(directory, header).map_err(journal_error)?;
        Ok(Self {
            limits,
            journal: Mutex::new(journal),
            tree_observer: Mutex::new(None),
        })
    }

    /// Opens and strictly replays one existing Spine sidecar journal.
    pub fn open(
        limits: SpineRuntimeLimits,
        directory: &Path,
        root_session_id: &str,
    ) -> Result<Self, SpineRuntimeError> {
        let journal = Journal::open(directory, root_session_id).map_err(journal_error)?;
        Ok(Self {
            limits,
            journal: Mutex::new(journal),
            tree_observer: Mutex::new(None),
        })
    }

    /// Returns the durable Spine root session ID.
    pub fn root_session_id(&self) -> Result<String, SpineRuntimeError> {
        self.with_journal(|journal| Ok(journal.state().header().root_session_id().to_owned()))
    }

    /// Returns the durable root prompt-cache key.
    pub fn prompt_cache_key(&self) -> Result<String, SpineRuntimeError> {
        self.with_journal(|journal| Ok(journal.state().header().prompt_cache_key().to_owned()))
    }

    /// Returns the only Nano session eligible to accept ordinary commands.
    pub fn active_session_id(&self) -> Result<String, SpineRuntimeError> {
        self.with_journal(|journal| Ok(journal.state().active_session_id().to_owned()))
    }

    /// Validates and synchronizes a prepared terminal control without moving
    /// the reducer cursor or the active Nano session.
    pub fn prepare(
        &self,
        request: SpineIntentRequest,
    ) -> Result<SpineTransition, SpineRuntimeError> {
        let transition = self.with_journal_mut(|journal| {
            if request.source_session_id != journal.state().active_session_id() {
                return Err(SpineRuntimeError::InactiveSession);
            }
            let intent = self.intent_for_request(journal, request)?;
            self.validate_prepared_context(journal, &intent)?;
            journal.prepare(intent.clone()).map_err(journal_error)?;
            Ok(SpineTransition { intent })
        })?;
        Ok(transition)
    }

    /// Returns the prepared transition represented by a durable terminal receipt.
    pub fn transition_for_receipt(
        &self,
        source_session_id: &str,
        receipt: &TerminalToolReceipt,
    ) -> Result<SpineTransition, SpineRuntimeError> {
        let control = terminal_control_from_receipt(receipt)?;
        self.with_journal(|journal| {
            let pending = journal
                .state()
                .pending()
                .cloned()
                .ok_or(SpineRuntimeError::MissingPreparedTransition)?;
            if pending.source_session_id() != source_session_id
                || pending.terminal_call_id() != receipt.call_id()
                || !intent_matches_control(&pending, &control)
            {
                return Err(SpineRuntimeError::PreparedTransitionMismatch);
            }
            Ok(SpineTransition { intent: pending })
        })
    }

    /// Commits a previously prepared transition after the target Nano session
    /// is durably available.
    pub fn commit(
        &self,
        transition: &SpineTransition,
        active_session_id: impl Into<String>,
        closed_session_id: Option<String>,
        delivery_id: impl Into<String>,
    ) -> Result<SpineDelivery, SpineRuntimeError> {
        let active_session_id = active_session_id.into();
        let delivery_id = delivery_id.into();
        self.render_continuation(&transition.intent, &delivery_id)?;
        let (delivery, tree) = self.with_journal_mut(|journal| {
            journal
                .commit(
                    transition.intent.clone(),
                    active_session_id.clone(),
                    closed_session_id,
                    delivery_id.clone(),
                )
                .map_err(journal_error)?;
            let tree = spine_tree_snapshot(&journal.state().projection());
            Ok((
                SpineDelivery {
                    id: delivery_id,
                    target_session_id: active_session_id,
                    kind: SpineDeliveryKind::Continuation,
                    transition: transition.clone(),
                },
                tree,
            ))
        })?;
        self.publish_tree(tree)?;
        Ok(delivery)
    }

    /// Resolves the sole prepared transition without changing the active node.
    pub fn abort_prepared(
        &self,
        transition: &SpineTransition,
        reason: SpineAbortReason,
        recovery_delivery_id: Option<String>,
    ) -> Result<Option<SpineDelivery>, SpineRuntimeError> {
        let (delivery, tree) = self.with_journal_mut(|journal| {
            let target_session_id = journal.state().active_session_id().to_owned();
            journal
                .abort(
                    transition.intent.clone(),
                    reason,
                    recovery_delivery_id.clone(),
                )
                .map_err(journal_error)?;
            let delivery = recovery_delivery_id.map(|id| SpineDelivery {
                id,
                target_session_id,
                kind: SpineDeliveryKind::Recovery,
                transition: transition.clone(),
            });
            let tree = spine_tree_snapshot(&journal.state().projection());
            Ok((delivery, tree))
        })?;
        self.publish_tree(tree)?;
        Ok(delivery)
    }

    /// Returns the unresolved prepared transition, if a crash interrupted one.
    pub fn pending_transition(&self) -> Result<Option<SpineTransition>, SpineRuntimeError> {
        self.with_journal(|journal| {
            Ok(journal
                .state()
                .pending()
                .cloned()
                .map(|intent| SpineTransition { intent }))
        })
    }

    /// Synchronizes the claim made before accepting the delivery prompt.
    pub fn claim_delivery(&self, delivery: &SpineDelivery) -> Result<(), SpineRuntimeError> {
        self.with_journal_mut(|journal| {
            journal
                .claim_delivery(
                    delivery.id.clone(),
                    &delivery.transition.intent,
                    delivery.target_session_id.clone(),
                    delivery_kind(delivery.kind),
                )
                .map_err(journal_error)
        })
    }

    /// Synchronizes that Nano accepted the delivery prompt command.
    pub fn accept_delivery(&self, delivery: &SpineDelivery) -> Result<(), SpineRuntimeError> {
        self.with_journal_mut(|journal| {
            journal
                .accept_delivery(delivery.id.clone(), delivery.target_session_id.clone())
                .map_err(journal_error)
        })
    }

    /// Returns the durable delivery progress for one known delivery ID.
    pub fn delivery_status(
        &self,
        delivery_id: &str,
    ) -> Result<Option<SpineDeliveryStatus>, SpineRuntimeError> {
        self.with_journal(|journal| {
            Ok(journal
                .state()
                .delivery_status(delivery_id)
                .map(delivery_status))
        })
    }

    /// Returns known delivery IDs targeting the durable active session.
    ///
    /// Applications can use these IDs to distinguish application-generated
    /// Spine continuations from ordinary user prompts when rebuilding a
    /// transcript, including a delivery recovered after an interrupted sync.
    pub fn active_delivery_ids(&self) -> Result<BTreeSet<String>, SpineRuntimeError> {
        self.with_journal(|journal| Ok(journal.state().active_delivery_ids()))
    }

    /// Returns the sole undelivered prompt targeting the current active node.
    pub fn unclaimed_active_delivery(&self) -> Result<Option<SpineDelivery>, SpineRuntimeError> {
        self.with_journal(|journal| {
            Ok(journal
                .state()
                .unclaimed_active_delivery()
                .map(spine_delivery))
        })
    }

    /// Returns an interrupted delivery that must be shown for manual resubmission.
    pub fn claimed_active_delivery(&self) -> Result<Option<SpineDelivery>, SpineRuntimeError> {
        self.with_journal(|journal| {
            Ok(journal
                .state()
                .claimed_active_delivery()
                .map(spine_delivery))
        })
    }

    /// Renders the bounded application-generated continuation prompt.
    pub fn delivery_prompt(&self, delivery: &SpineDelivery) -> Result<String, SpineRuntimeError> {
        match delivery.kind {
            SpineDeliveryKind::Continuation => {
                self.render_continuation(&delivery.transition.intent, &delivery.id)
            }
            SpineDeliveryKind::Recovery => self.render_recovery(&delivery.id),
        }
    }

    /// Returns the journal-derived reducer projection.
    pub fn projection(&self) -> Result<SpineProjection, SpineRuntimeError> {
        self.with_journal(|journal| Ok(journal.state().projection()))
    }

    /// Replaces the live UI observer and immediately sends it the current tree.
    pub fn set_tree_observer(&self, observer: SpineTreeObserver) -> Result<(), SpineRuntimeError> {
        let tree =
            self.with_journal(|journal| Ok(spine_tree_snapshot(&journal.state().projection())))?;
        let mut stored = self
            .tree_observer
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)?;
        *stored = Some(Arc::clone(&observer));
        drop(stored);
        observer(tree);
        Ok(())
    }

    fn intent_for_request(
        &self,
        journal: &Journal,
        request: SpineIntentRequest,
    ) -> Result<TransitionIntent, SpineRuntimeError> {
        let parent_session_id = || {
            journal
                .state()
                .active_parent_session_id()
                .ok_or(SpineRuntimeError::NoLiveTask)
        };
        match request.control {
            SpineTerminalControl::Open { summary } => Ok(TransitionIntent::open(
                request.source_session_id,
                request.terminal_call_id,
                summary,
            )),
            SpineTerminalControl::Close { memory } => Ok(TransitionIntent::close(
                request.source_session_id,
                request.terminal_call_id,
                parent_session_id()?,
                memory,
            )),
            SpineTerminalControl::Next { summary, memory } => Ok(TransitionIntent::next(
                request.source_session_id,
                request.terminal_call_id,
                parent_session_id()?,
                summary,
                memory,
            )),
        }
    }

    fn validate_prepared_context(
        &self,
        journal: &Journal,
        intent: &TransitionIntent,
    ) -> Result<(), SpineRuntimeError> {
        match intent.kind() {
            TransitionKind::Open => {
                self.validate_summary(intent.summary().unwrap_or_default())?;
                self.ensure_capacity(journal, true)?;
            }
            TransitionKind::Close => {
                self.validate_memory(intent.memory().unwrap_or_default())?;
            }
            TransitionKind::Next => {
                self.validate_summary(intent.summary().unwrap_or_default())?;
                self.validate_memory(intent.memory().unwrap_or_default())?;
                self.ensure_capacity(journal, false)?;
            }
        }
        self.render_continuation(intent, &"d".repeat(MAX_DELIVERY_ID_BYTES))?;
        Ok(())
    }

    fn ensure_capacity(
        &self,
        journal: &Journal,
        opening_child: bool,
    ) -> Result<(), SpineRuntimeError> {
        let projection = journal.state().projection();
        let depth = projection.cursor.parts().len().saturating_sub(1);
        if opening_child && depth >= self.limits.max_depth as usize {
            return Err(SpineRuntimeError::DepthLimit(self.limits.max_depth));
        }
        if journal.state().task_node_count() >= self.limits.max_nodes {
            return Err(SpineRuntimeError::NodeLimit(self.limits.max_nodes));
        }
        Ok(())
    }

    fn validate_summary(&self, summary: &str) -> Result<(), SpineRuntimeError> {
        let summary = required(summary, SpineRuntimeError::EmptySummary)?;
        bounded(
            summary,
            self.limits
                .max_summary_bytes
                .min(MAX_CONTINUATION_CONTEXT_BYTES),
            SpineRuntimeError::SummaryTooLarge(
                self.limits
                    .max_summary_bytes
                    .min(MAX_CONTINUATION_CONTEXT_BYTES),
            ),
        )
    }

    fn validate_memory(&self, memory: &str) -> Result<(), SpineRuntimeError> {
        let memory = required(memory, SpineRuntimeError::EmptyMemory)?;
        bounded(
            memory,
            self.limits.max_memory_bytes.min(MAX_HANDOFF_MEMORY_BYTES),
            SpineRuntimeError::MemoryTooLarge(
                self.limits.max_memory_bytes.min(MAX_HANDOFF_MEMORY_BYTES),
            ),
        )
    }

    fn render_continuation(
        &self,
        intent: &TransitionIntent,
        delivery_id: &str,
    ) -> Result<String, SpineRuntimeError> {
        let marker = format!("<spine_delivery id=\"{delivery_id}\">");
        let prompt = match intent.kind() {
            TransitionKind::Open => format!(
                "{marker}\nYou now own one focused Spine continuation scope. Work only on this scope, then call tools.spine__close({{memory: ...}}) with a compact, evidence-backed handoff. If the scope genuinely changes but its frozen parent should remain blocked, call tools.spine__next({{summary: ..., memory: ...}}). Do not finish with a normal assistant message.\n\nScope:\n{}\n</spine_delivery>",
                intent.summary().unwrap_or_default().trim()
            ),
            TransitionKind::Close => format!(
                "{marker}\nYou resumed the parent Spine scope. A child finished with this compact handoff:\n<spine_memory>\n{}\n</spine_memory>\nContinue the parent scope.\n</spine_delivery>",
                intent.memory().unwrap_or_default().trim()
            ),
            TransitionKind::Next => format!(
                "{marker}\nYou now own a sibling Spine continuation scope. The previous sibling handed off:\n<spine_memory>\n{}\n</spine_memory>\nWork only on this scope, then call tools.spine__close({{memory: ...}}) or tools.spine__next({{summary: ..., memory: ...}}). Do not finish with a normal assistant message.\n\nScope:\n{}\n</spine_delivery>",
                intent.memory().unwrap_or_default().trim(),
                intent.summary().unwrap_or_default().trim()
            ),
        };
        bounded(
            &prompt,
            MAX_CONTINUATION_CONTEXT_BYTES,
            SpineRuntimeError::ContinuationContextTooLarge(MAX_CONTINUATION_CONTEXT_BYTES),
        )?;
        Ok(prompt)
    }

    fn render_recovery(&self, delivery_id: &str) -> Result<String, SpineRuntimeError> {
        let prompt = format!(
            "<spine_delivery id=\"{delivery_id}\">\nThe previous Spine control did not commit. Continue from this current scope, and issue a new Spine control only if it is still needed.\n</spine_delivery>"
        );
        bounded(
            &prompt,
            MAX_CONTINUATION_CONTEXT_BYTES,
            SpineRuntimeError::ContinuationContextTooLarge(MAX_CONTINUATION_CONTEXT_BYTES),
        )?;
        Ok(prompt)
    }

    fn with_journal<T>(
        &self,
        operation: impl FnOnce(&Journal) -> Result<T, SpineRuntimeError>,
    ) -> Result<T, SpineRuntimeError> {
        let journal = self
            .journal
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)?;
        operation(&journal)
    }

    fn with_journal_mut<T>(
        &self,
        operation: impl FnOnce(&mut Journal) -> Result<T, SpineRuntimeError>,
    ) -> Result<T, SpineRuntimeError> {
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| SpineRuntimeError::StateUnavailable)?;
        operation(&mut journal)
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

fn terminal_control_from_receipt(
    receipt: &TerminalToolReceipt,
) -> Result<SpineTerminalControl, SpineRuntimeError> {
    let metadata = receipt
        .metadata()
        .ok_or(SpineRuntimeError::InvalidTerminalReceipt)?;
    let control = serde_json::from_str::<SpineTerminalControl>(metadata.get())
        .map_err(|_| SpineRuntimeError::InvalidTerminalReceipt)?;
    let tool_matches = matches!(
        (receipt.tool_name(), &control),
        ("spine__open", SpineTerminalControl::Open { .. })
            | ("spine__close", SpineTerminalControl::Close { .. })
            | ("spine__next", SpineTerminalControl::Next { .. })
    );
    tool_matches
        .then_some(control)
        .ok_or(SpineRuntimeError::ReceiptToolMismatch)
}

fn intent_matches_control(intent: &TransitionIntent, control: &SpineTerminalControl) -> bool {
    match (intent.kind(), control) {
        (TransitionKind::Open, SpineTerminalControl::Open { summary }) => {
            intent.summary() == Some(summary)
        }
        (TransitionKind::Close, SpineTerminalControl::Close { memory }) => {
            intent.memory() == Some(memory)
        }
        (TransitionKind::Next, SpineTerminalControl::Next { summary, memory }) => {
            intent.summary() == Some(summary) && intent.memory() == Some(memory)
        }
        _ => false,
    }
}

const fn delivery_kind(kind: SpineDeliveryKind) -> DeliveryKind {
    match kind {
        SpineDeliveryKind::Continuation => DeliveryKind::Continuation,
        SpineDeliveryKind::Recovery => DeliveryKind::Recovery,
    }
}

const fn delivery_status(status: DeliveryStatus) -> SpineDeliveryStatus {
    match status {
        DeliveryStatus::Unclaimed => SpineDeliveryStatus::Unclaimed,
        DeliveryStatus::Claimed => SpineDeliveryStatus::Claimed,
        DeliveryStatus::Accepted => SpineDeliveryStatus::Accepted,
    }
}

fn spine_delivery(
    (id, target_session_id, intent, kind): (String, String, TransitionIntent, DeliveryKind),
) -> SpineDelivery {
    SpineDelivery {
        id,
        target_session_id,
        kind: match kind {
            DeliveryKind::Continuation => SpineDeliveryKind::Continuation,
            DeliveryKind::Recovery => SpineDeliveryKind::Recovery,
        },
        transition: SpineTransition { intent },
    }
}

fn journal_error(error: JournalError) -> SpineRuntimeError {
    SpineRuntimeError::Journal(error.to_string())
}

fn required(value: &str, error: SpineRuntimeError) -> Result<&str, SpineRuntimeError> {
    (!value.trim().is_empty()).then_some(value).ok_or(error)
}

fn bounded(value: &str, maximum: usize, error: SpineRuntimeError) -> Result<(), SpineRuntimeError> {
    (value.len() <= maximum).then_some(()).ok_or(error)
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
