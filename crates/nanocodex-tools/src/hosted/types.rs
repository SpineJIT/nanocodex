use std::sync::Arc;

use nanocodex_oai_api::{
    responses::ResponseItem,
    tools::{
        TerminalToolReceipt, TerminalToolReceiptError, ToolContext, ToolOutputBody,
        ToolTurnBehavior,
    },
};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};

/// Owned context for a Code Mode cell that may outlive its initiating call.
///
/// Prefer [`ToolContext`] for ordinary synchronous tool handlers. This owned
/// form retains shared history without copying it again when execution crosses
/// an asynchronous host boundary.
pub struct OwnedToolContext {
    pub(crate) model: String,
    pub(crate) session_id: String,
    pub(crate) call_id: String,
    pub(crate) history: Arc<Vec<ResponseItem>>,
    pub(crate) output_token_budget: usize,
}

impl OwnedToolContext {
    /// Creates an owned context from its complete invocation state.
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        session_id: impl Into<String>,
        call_id: impl Into<String>,
        history: Arc<Vec<ResponseItem>>,
        output_token_budget: usize,
    ) -> Self {
        Self {
            model: model.into(),
            session_id: session_id.into(),
            call_id: call_id.into(),
            history,
            output_token_budget,
        }
    }

    /// Copies a borrowed context into independently owned invocation state.
    #[must_use]
    pub fn from_context(context: ToolContext<'_>) -> Self {
        Self::new(
            context.model(),
            context.session_id(),
            context.call_id(),
            Arc::new(context.history().to_vec()),
            context.output_token_budget(),
        )
    }

    /// Borrows this owned state as the standard tool invocation context.
    #[must_use]
    pub fn as_context(&self) -> ToolContext<'_> {
        ToolContext::new(
            &self.model,
            &self.session_id,
            &self.call_id,
            self.history.as_slice(),
            self.output_token_budget,
        )
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) const fn with_output_token_budget(mut self, output_token_budget: usize) -> Self {
        self.output_token_budget = output_token_budget;
        self
    }
}

/// Complete result of one Code Mode cell observation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeModeExecution {
    /// Ordered model-visible output emitted by the cell.
    pub output: ToolOutputBody,
    /// Whether the JavaScript cell reached a successful terminal state.
    pub success: bool,
    /// Nested tool calls in their original invocation order.
    #[serde(default)]
    pub nested_calls: Vec<NestedToolCall>,
    /// Application notifications emitted by the cell.
    #[serde(default)]
    pub notifications: Vec<CodeModeNotification>,
    #[serde(skip)]
    pub(crate) terminal_selection: TerminalToolSelection,
}

impl CodeModeExecution {
    /// Creates one complete Code Mode execution result with no terminal tool selected.
    #[must_use]
    pub const fn new(
        output: ToolOutputBody,
        success: bool,
        nested_calls: Vec<NestedToolCall>,
        notifications: Vec<CodeModeNotification>,
    ) -> Self {
        Self {
            output,
            success,
            nested_calls,
            notifications,
            terminal_selection: TerminalToolSelection::None,
        }
    }

    /// Returns the one successful terminal tool selected after this cell completed.
    #[must_use]
    pub const fn terminal_tool(&self) -> Option<&TerminalToolReceipt> {
        match &self.terminal_selection {
            TerminalToolSelection::None
            | TerminalToolSelection::Ambiguous
            | TerminalToolSelection::Invalid(_) => None,
            TerminalToolSelection::Selected(receipt) => Some(receipt),
        }
    }

    /// Returns whether this cell selected more than one successful terminal tool.
    #[must_use]
    pub const fn has_ambiguous_terminal_tool(&self) -> bool {
        matches!(self.terminal_selection, TerminalToolSelection::Ambiguous)
    }

    /// Returns the receipt validation error that prevented terminal completion, if any.
    #[must_use]
    pub const fn terminal_receipt_error(&self) -> Option<TerminalToolReceiptError> {
        match self.terminal_selection {
            TerminalToolSelection::Invalid(error) => Some(error),
            TerminalToolSelection::None
            | TerminalToolSelection::Selected(_)
            | TerminalToolSelection::Ambiguous => None,
        }
    }

    pub(crate) fn select_terminal_tools(
        &mut self,
        candidates: Vec<Result<TerminalToolReceipt, TerminalToolReceiptError>>,
    ) {
        let mut receipts = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            match candidate {
                Ok(receipt) => receipts.push(receipt),
                Err(error) => {
                    self.terminal_selection = TerminalToolSelection::Invalid(error);
                    return;
                }
            }
        }
        self.terminal_selection = match receipts.len() {
            0 => TerminalToolSelection::None,
            1 => match receipts.into_iter().next() {
                Some(receipt) => TerminalToolSelection::Selected(receipt),
                None => TerminalToolSelection::None,
            },
            _ => TerminalToolSelection::Ambiguous,
        };
    }

    pub(crate) fn apply_host_turn_behaviors(
        &mut self,
        turn_behavior: impl Fn(&str) -> ToolTurnBehavior,
        emits_output_on_success: impl Fn(&str) -> bool,
    ) {
        for call in &mut self.nested_calls {
            call.turn_behavior = turn_behavior(&call.name);
        }
        if let Some(output) = crate::emitted_output::bounded_emitted_output(
            self.nested_calls
                .iter()
                .filter(|call| call.success && emits_output_on_success(&call.name))
                .map(|call| &call.output),
        ) {
            crate::emitted_output::append_bounded_output(&mut self.output, output);
        }
        let candidates = if self.success {
            self.nested_calls
                .iter()
                .filter(|call| {
                    call.success && call.turn_behavior == ToolTurnBehavior::FinishTurnOnSuccess
                })
                .map(|call| {
                    TerminalToolReceipt::new(
                        call.call_id.clone(),
                        call.name.clone(),
                        call.output.clone(),
                        call.metadata.clone(),
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        self.select_terminal_tools(candidates);
    }
}

#[derive(Debug, Default)]
pub(crate) enum TerminalToolSelection {
    #[default]
    None,
    Selected(TerminalToolReceipt),
    Ambiguous,
    Invalid(TerminalToolReceiptError),
}

/// One notification emitted by a Code Mode cell.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeModeNotification {
    /// Code Mode call that emitted the notification.
    pub call_id: String,
    /// Complete notification text.
    pub text: String,
}

impl CodeModeNotification {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn new(call_id: &str, text: String) -> Self {
        Self {
            call_id: call_id.to_owned(),
            text,
        }
    }
}

/// Incremental nested-tool update observed while a Code Mode cell runs.
pub enum CodeModeUpdate<'a> {
    /// A nested call was accepted and may now run concurrently.
    NestedCallStarted {
        /// Stable nested call identity.
        call_id: &'a str,
        /// Registered tool name.
        name: &'a str,
        /// Complete JSON input value.
        input: &'a Value,
    },
    /// A nested call reached a terminal result.
    NestedCallCompleted(&'a NestedToolCall),
}

/// Observer for ordered nested-tool lifecycle updates.
///
/// The callback runs inline with Code Mode observation. Implementations should
/// hand off expensive work rather than blocking the cell.
pub trait CodeModeObserver: Send {
    /// Observes one ordered nested-tool update.
    fn update(&mut self, update: CodeModeUpdate<'_>);
}

/// Recorded nested tool call made by one Code Mode cell.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NestedToolCall {
    /// Stable call identity derived from the parent Code Mode invocation.
    pub call_id: String,
    /// Registered tool name.
    pub name: String,
    /// Complete JSON input value.
    pub input: Value,
    /// Complete model-visible output.
    pub output: ToolOutputBody,
    /// Whether the nested operation succeeded.
    pub success: bool,
    /// Static turn behavior declared by the invoked tool.
    #[serde(default)]
    pub turn_behavior: ToolTurnBehavior,
    /// Nanoseconds from cell start until this call started.
    pub started_after_ns: u64,
    /// Nanoseconds spent executing this call.
    pub duration_ns: u64,
    /// Optional opaque metadata retained for events and adapters.
    #[serde(default)]
    pub metadata: Option<Box<RawValue>>,
}
