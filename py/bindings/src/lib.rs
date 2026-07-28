use std::sync::{Arc, Mutex};

use nanocodex::{
    AgentEvents as RustAgentEvents, Nanocodex as RustNanocodex, OpenAi, ReasoningMode, Thinking,
    TurnControl as RustTurnControl, TurnResult as RustTurnResult,
    oai::auth::{OpenAiAuth, load_chatgpt_auth},
};
use pyo3::{
    Bound, PyResult, Python,
    exceptions::{PyRuntimeError, PyValueError},
    prelude::{Py, PyModule, pyclass, pymethods, pymodule},
    types::{PyDict, PyDictMethods, PyModuleMethods},
};
use tokio::runtime::Runtime;

#[pyclass(frozen, module = "nanocodex._native")]
struct Nanocodex {
    runtime: Arc<Runtime>,
    agent: RustNanocodex,
}

#[pymethods]
impl Nanocodex {
    #[new]
    #[pyo3(signature = (api_key = None, *, auth_file = None, thinking = "high", reasoning_mode = "standard", fast_mode = false, workspace = None, instructions = None))]
    #[allow(
        clippy::too_many_arguments,
        reason = "PyO3 exposes one flat keyword-only options surface"
    )]
    fn new(
        api_key: Option<String>,
        auth_file: Option<String>,
        thinking: &str,
        reasoning_mode: &str,
        fast_mode: bool,
        workspace: Option<String>,
        instructions: Option<String>,
    ) -> PyResult<(Self, AgentEvents)> {
        let auth = match (api_key, auth_file) {
            (Some(api_key), None) => OpenAiAuth::api_key(api_key),
            (None, Some(auth_file)) => load_chatgpt_auth(auth_file).map_err(runtime_error)?,
            (Some(_), Some(_)) => {
                return Err(PyValueError::new_err(
                    "pass either api_key or auth_file, not both",
                ));
            }
            (None, None) => {
                return Err(PyValueError::new_err("api_key or auth_file is required"));
            }
        };
        let thinking = parse_thinking(thinking)?;
        let reasoning_mode = parse_reasoning_mode(reasoning_mode)?;
        let runtime = build_runtime()?;
        let openai = OpenAi::new(auth).map_err(runtime_error)?;
        let (agent, events) = runtime
            .block_on(async move {
                let mut builder = RustNanocodex::builder(openai)
                    .reasoning_mode(reasoning_mode)
                    .thinking(thinking)
                    .fast_mode(fast_mode);
                if let Some(workspace) = workspace {
                    builder = builder.workspace(workspace);
                }
                if let Some(instructions) = instructions {
                    builder = builder.instructions(instructions);
                }
                builder.build()
            })
            .map_err(runtime_error)?;
        Ok(wrap_agent(runtime, agent, events))
    }

    /// Accept a prompt and immediately return its independently awaitable turn.
    fn prompt(&self, py: Python<'_>, prompt: String) -> PyResult<Turn> {
        let runtime = Arc::clone(&self.runtime);
        let agent = self.agent.clone();
        let turn = py
            .detach(move || runtime.block_on(agent.prompt(prompt)))
            .map_err(runtime_error)?;
        Ok(Turn {
            runtime: Arc::clone(&self.runtime),
            control: turn.control(),
            state: Mutex::new(TurnState::Pending(turn)),
        })
    }

    /// Change the reasoning effort for subsequently accepted turns.
    fn set_thinking(&self, py: Python<'_>, thinking: &str) -> PyResult<()> {
        let thinking = parse_thinking(thinking)?;
        let runtime = Arc::clone(&self.runtime);
        let agent = self.agent.clone();
        py.detach(move || runtime.block_on(agent.set_thinking(thinking)))
            .map_err(runtime_error)
    }

    /// Enable or disable priority processing for subsequently accepted turns.
    fn set_fast_mode(&self, py: Python<'_>, enabled: bool) -> PyResult<()> {
        let runtime = Arc::clone(&self.runtime);
        let agent = self.agent.clone();
        py.detach(move || runtime.block_on(agent.set_fast_mode(enabled)))
            .map_err(runtime_error)
    }

    /// Compact retained history immediately without fabricating a user prompt.
    fn compact(&self, py: Python<'_>) -> PyResult<()> {
        let runtime = Arc::clone(&self.runtime);
        let agent = self.agent.clone();
        py.detach(move || runtime.block_on(agent.compact()))
            .map_err(runtime_error)
    }

    /// Start a clean sibling agent with the same private configuration.
    ///
    /// The sibling does not inherit conversation history.
    fn spawn(&self, py: Python<'_>) -> PyResult<(Self, AgentEvents)> {
        let runtime = Arc::clone(&self.runtime);
        let agent = self.agent.clone();
        let (child, events) = py
            .detach(move || runtime.block_on(agent.spawn()))
            .map_err(runtime_error)?;
        Ok(wrap_agent(Arc::clone(&self.runtime), child, events))
    }

    /// Fork from the latest safe model boundary into an independently driven agent.
    fn fork(&self, py: Python<'_>) -> PyResult<(Self, AgentEvents)> {
        let runtime = Arc::clone(&self.runtime);
        let agent = self.agent.clone();
        let (child, events) = py
            .detach(move || runtime.block_on(agent.fork()))
            .map_err(runtime_error)?;
        Ok(wrap_agent(Arc::clone(&self.runtime), child, events))
    }

    /// Fork from the exact checkpoint retained by a completed historical turn.
    fn fork_from(&self, py: Python<'_>, turn: &Turn) -> PyResult<(Self, AgentEvents)> {
        let completed = turn.completed_result()?;
        let runtime = Arc::clone(&self.runtime);
        let agent = self.agent.clone();
        let (child, events) = py
            .detach(move || runtime.block_on(agent.fork_from(&completed)))
            .map_err(runtime_error)?;
        Ok(wrap_agent(Arc::clone(&self.runtime), child, events))
    }

    fn __repr__(&self) -> String {
        format!(
            "Nanocodex(runtime_references={})",
            Arc::strong_count(&self.runtime)
        )
    }
}

#[pyclass(module = "nanocodex._native")]
struct Turn {
    runtime: Arc<Runtime>,
    control: RustTurnControl,
    state: Mutex<TurnState>,
}

enum TurnState {
    Pending(nanocodex::Turn),
    Waiting,
    Completed(RustTurnResult),
    Failed(String),
}

impl Turn {
    fn completed_result(&self) -> PyResult<RustTurnResult> {
        let state = self.state.lock().map_err(lock_error)?;
        match &*state {
            TurnState::Completed(result) => Ok(result.clone()),
            TurnState::Pending(_) | TurnState::Waiting => Err(PyRuntimeError::new_err(
                "turn has not completed; await result() before fork_from",
            )),
            TurnState::Failed(error) => Err(PyRuntimeError::new_err(format!(
                "turn failed and has no checkpoint: {error}"
            ))),
        }
    }
}

#[pymethods]
impl Turn {
    /// Inject additional input into this turn at its next safe model boundary.
    fn steer(&self, py: Python<'_>, instruction: String) -> PyResult<()> {
        let runtime = Arc::clone(&self.runtime);
        let control = self.control.clone();
        py.detach(move || runtime.block_on(control.steer(instruction)))
            .map_err(runtime_error)
    }

    /// Cancel this exact unfinished turn.
    fn cancel(&self, py: Python<'_>) -> PyResult<()> {
        let runtime = Arc::clone(&self.runtime);
        let control = self.control.clone();
        py.detach(move || runtime.block_on(control.cancel()))
            .map_err(runtime_error)
    }

    /// Block until the turn completes and return its final assistant message.
    fn result(&self, py: Python<'_>) -> PyResult<String> {
        let turn = {
            let mut state = self.state.lock().map_err(lock_error)?;
            match &*state {
                TurnState::Completed(result) => return Ok(result.final_message().to_owned()),
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
                let message = result.final_message().to_owned();
                *self.state.lock().map_err(lock_error)? = TurnState::Completed(result);
                Ok(message)
            }
            Err(error) => {
                let error = error.to_string();
                *self.state.lock().map_err(lock_error)? = TurnState::Failed(error.clone());
                Err(PyRuntimeError::new_err(error))
            }
        }
    }

    /// Return exact aggregate token usage for this completed logical turn.
    fn usage(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        let state = self.state.lock().map_err(lock_error)?;
        let result = match &*state {
            TurnState::Completed(result) => result,
            TurnState::Pending(_) | TurnState::Waiting => {
                return Err(PyRuntimeError::new_err(
                    "turn has not completed; await result() before reading usage",
                ));
            }
            TurnState::Failed(error) => {
                return Err(PyRuntimeError::new_err(format!(
                    "turn failed and has no usage: {error}"
                )));
            }
        };
        let usage = result.usage();
        let output = PyDict::new(py);
        output.set_item("input_tokens", usage.input_tokens())?;
        output.set_item("cached_input_tokens", usage.cached_input_tokens())?;
        output.set_item("cache_write_input_tokens", usage.cache_write_input_tokens())?;
        output.set_item("output_tokens", usage.output_tokens())?;
        output.set_item("reasoning_output_tokens", usage.reasoning_output_tokens())?;
        output.set_item("total_tokens", usage.total_tokens())?;
        if let Some(cost) = usage.estimated_cost() {
            output.set_item("estimated_cost", estimated_cost_dict(py, cost)?)?;
        } else {
            output.set_item("estimated_cost", py.None())?;
        }
        output.set_item("cost_status", usage.cost_status().as_str())?;
        Ok(output.unbind())
    }
}

#[pyclass(frozen, module = "nanocodex._native")]
struct AgentEvents {
    runtime: Arc<Runtime>,
    events: Arc<tokio::sync::Mutex<RustAgentEvents>>,
}

#[pymethods]
impl AgentEvents {
    /// Block for one event and return its exact JSON representation.
    fn recv_json(&self, py: Python<'_>) -> PyResult<Option<String>> {
        let runtime = Arc::clone(&self.runtime);
        let events = Arc::clone(&self.events);
        let event =
            py.detach(move || runtime.block_on(async move { events.lock().await.recv().await }));
        event
            .map(|event| serde_json::to_string(&event).map_err(runtime_error))
            .transpose()
    }
}

fn wrap_agent(
    runtime: Arc<Runtime>,
    agent: RustNanocodex,
    events: RustAgentEvents,
) -> (Nanocodex, AgentEvents) {
    (
        Nanocodex {
            runtime: Arc::clone(&runtime),
            agent,
        },
        AgentEvents {
            runtime,
            events: Arc::new(tokio::sync::Mutex::new(events)),
        },
    )
}

fn build_runtime() -> PyResult<Arc<Runtime>> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map(Arc::new)
        .map_err(runtime_error)
}

fn parse_thinking(value: &str) -> PyResult<Thinking> {
    value.parse().map_err(PyValueError::new_err)
}

fn parse_reasoning_mode(value: &str) -> PyResult<ReasoningMode> {
    value.parse().map_err(PyValueError::new_err)
}

fn estimated_cost_dict<'py>(
    py: Python<'py>,
    cost: &nanocodex::oai::pricing::EstimatedUsdCost,
) -> PyResult<Bound<'py, PyDict>> {
    let output = PyDict::new(py);
    output.set_item("usd", cost.amount().decimal())?;
    output.set_item("input_usd", cost.input().decimal())?;
    output.set_item("cached_input_usd", cost.cached_input().decimal())?;
    output.set_item("cache_write_input_usd", cost.cache_write_input().decimal())?;
    output.set_item("output_usd", cost.output().decimal())?;
    output.set_item("service_tier", cost.service_tier().as_str())?;
    Ok(output)
}

#[allow(clippy::needless_pass_by_value)]
fn runtime_error(error: impl ToString) -> pyo3::PyErr {
    PyRuntimeError::new_err(error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn lock_error<T>(error: std::sync::PoisonError<T>) -> pyo3::PyErr {
    PyRuntimeError::new_err(format!("binding state lock was poisoned: {error}"))
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Nanocodex>()?;
    module.add_class::<Turn>()?;
    module.add_class::<AgentEvents>()?;
    Ok(())
}
