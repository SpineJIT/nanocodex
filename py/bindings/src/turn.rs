use std::sync::{Arc, Mutex};

use nanocodex::{
    TurnCompletion, TurnControl as RustTurnControl, TurnResult as RustTurnResult,
    agent::Turn as RustTurn,
};
use pyo3::{
    Py, PyResult, Python,
    exceptions::PyRuntimeError,
    prelude::{pyclass, pymethods},
    types::{PyDict, PyDictMethods},
};
use tokio::runtime::Runtime;

use crate::{
    error::{lock_error, runtime_error},
    events::json_to_python,
    snapshot::SessionSnapshot,
    usage::usage_dict,
};

/// Independently controllable completion handle for one accepted turn.
#[pyclass(module = "nanocodex._native")]
pub(crate) struct Turn {
    runtime: Arc<Runtime>,
    control: Mutex<Option<RustTurnControl>>,
    state: Mutex<TurnState>,
}

enum TurnState {
    Pending(RustTurn),
    Waiting,
    Completed(RustTurnResult),
    Failed(String),
}

#[pymethods]
impl Turn {
    /// Inject additional input at this turn's next safe model boundary.
    fn steer(&self, py: Python<'_>, instruction: String) -> PyResult<()> {
        let runtime = Arc::clone(&self.runtime);
        let control = self.control()?;
        py.detach(move || runtime.block_on(control.steer(instruction)))
            .map_err(runtime_error)
    }

    /// Cancel this exact unfinished turn.
    fn cancel(&self, py: Python<'_>) -> PyResult<()> {
        let runtime = Arc::clone(&self.runtime);
        let control = self.control()?;
        py.detach(move || runtime.block_on(control.cancel()))
            .map_err(runtime_error)
    }

    /// Block until completion and return the typed result.
    fn result(&self, py: Python<'_>) -> PyResult<TurnResult> {
        let turn = {
            let mut state = self.state.lock().map_err(lock_error)?;
            match &*state {
                TurnState::Completed(result) => return Ok(TurnResult::new(result.clone())),
                TurnState::Failed(error) => return Err(PyRuntimeError::new_err(error.clone())),
                TurnState::Waiting => {
                    return Err(PyRuntimeError::new_err(
                        "another thread is already waiting for this turn",
                    ));
                }
                TurnState::Pending(_) => {}
            }
            match std::mem::replace(&mut *state, TurnState::Waiting) {
                TurnState::Pending(turn) => turn,
                _ => unreachable!("pending state was checked before replacement"),
            }
        };

        let runtime = Arc::clone(&self.runtime);
        match py.detach(move || runtime.block_on(turn.result())) {
            Ok(result) => {
                *self.state.lock().map_err(lock_error)? = TurnState::Completed(result.clone());
                self.control.lock().map_err(lock_error)?.take();
                Ok(TurnResult::new(result))
            }
            Err(error) => {
                let error = error.to_string();
                *self.state.lock().map_err(lock_error)? = TurnState::Failed(error.clone());
                self.control.lock().map_err(lock_error)?.take();
                Err(PyRuntimeError::new_err(error))
            }
        }
    }
}

impl Turn {
    pub(crate) fn new(runtime: Arc<Runtime>, turn: RustTurn) -> Self {
        let control = turn.control();
        Self {
            runtime,
            control: Mutex::new(Some(control)),
            state: Mutex::new(TurnState::Pending(turn)),
        }
    }

    fn control(&self) -> PyResult<RustTurnControl> {
        self.control
            .lock()
            .map_err(lock_error)?
            .as_ref()
            .cloned()
            .ok_or_else(|| PyRuntimeError::new_err("turn is already complete"))
    }
}

/// Final typed output, usage, cost, and checkpoint for a completed turn.
#[pyclass(frozen, module = "nanocodex._native")]
pub(crate) struct TurnResult {
    inner: RustTurnResult,
}

#[pymethods]
impl TurnResult {
    /// Semantic completion for this completed turn.
    #[getter]
    fn completion(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        let completion = PyDict::new(py);
        match self.inner.completion() {
            TurnCompletion::Message { final_message } => {
                completion.set_item("type", "message")?;
                completion.set_item("final_message", final_message)?;
            }
            TurnCompletion::TerminalTool { receipt } => {
                completion.set_item("type", "terminal_tool")?;
                let receipt_dict = PyDict::new(py);
                receipt_dict.set_item("call_id", receipt.call_id())?;
                receipt_dict.set_item("tool_name", receipt.tool_name())?;
                receipt_dict.set_item(
                    "output",
                    json_to_python(
                        py,
                        &serde_json::to_value(receipt.output()).map_err(runtime_error)?,
                    )?,
                )?;
                let metadata: Option<serde_json::Value> = receipt
                    .metadata()
                    .map(|metadata| serde_json::from_str(metadata.get()))
                    .transpose()
                    .map_err(runtime_error)?;
                receipt_dict.set_item(
                    "metadata",
                    metadata
                        .as_ref()
                        .map_or_else(|| Ok(py.None()), |value| json_to_python(py, value))?,
                )?;
                completion.set_item("receipt", receipt_dict)?;
            }
            _ => return Err(runtime_error("unknown turn completion")),
        }
        Ok(completion.unbind())
    }

    /// Final assistant message for this completed turn, when present.
    #[getter]
    fn final_message(&self) -> Option<String> {
        self.inner.final_message().map(ToOwned::to_owned)
    }

    /// Exact aggregate token usage and automatic USD estimate.
    fn usage(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        usage_dict(py, self.inner.usage())
    }

    /// Copy this completed boundary into a caller-owned session snapshot.
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot::new(self.inner.snapshot())
    }

    fn __repr__(&self) -> String {
        match self.inner.completion() {
            TurnCompletion::Message { final_message } => {
                format!("TurnResult(final_message_bytes={})", final_message.len())
            }
            TurnCompletion::TerminalTool { receipt } => {
                format!("TurnResult(terminal_tool='{}')", receipt.tool_name())
            }
            _ => "TurnResult(unknown_completion)".to_owned(),
        }
    }
}

impl TurnResult {
    const fn new(inner: RustTurnResult) -> Self {
        Self { inner }
    }

    pub(crate) const fn inner(&self) -> &RustTurnResult {
        &self.inner
    }
}
