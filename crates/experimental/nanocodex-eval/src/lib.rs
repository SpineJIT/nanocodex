//! Typed, VM-isolated evaluation for Nanocodex agents.
//!
//! This crate owns task loading, durable profile worksets, typed events and
//! outcomes, and VM-isolated execution. Applications choose one exact profile
//! coordinate family; SQLite allocates its internal repetition and fences the
//! accepted completion.
//!
//! # Run one task
//!
//! ```no_run
//! use nanocodex_agent::{Nanocodex, OpenAi, Thinking};
//! use nanocodex_eval::{Evaluator, Task, VmResources};
//!
//! # async fn evaluate() -> Result<(), Box<dyn std::error::Error>> {
//! let task = Task::load("tasks/write-greeting")?;
//! let resources = VmResources::builder("target/debug/nanocodex", ".cache/vm/runtime.ext4")
//!     .task(task.clone())
//!     .prepare()
//!     .await?;
//! let backend = resources.backend().await?;
//! let agent = Nanocodex::builder(OpenAi::new(std::env::var("OPENAI_API_KEY")?)?)
//!     .instructions(
//!         "Work directly in the provided workspace. Complete the requested \
//!          task, verify your changes, and keep the final answer concise.",
//!     )
//!     .thinking(Thinking::Medium);
//! let evaluator = Evaluator::builder(agent, backend)
//!     .output_directory(".nanocodex/evals")
//!     .build()?;
//! let run = evaluator.task(task);
//! let mut stream = run.events().subscribe();
//! let event_task = tokio::spawn(async move {
//!     while let Some(event) = stream.recv().await? {
//!         println!("{} {:?}", event.sequence, event.kind);
//!     }
//!     Ok::<_, nanocodex_eval::EvalEventStreamError>(())
//! });
//!
//! let results = run.await?;
//! println!("outcome: {:?}", results.outcome());
//! event_task.await??;
//! # Ok(())
//! # }
//! ```
//!
//! Every accepted attempt receives a fresh session and workspace. Results are
//! independently awaitable from the optional event stream.

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]
// Retained-data readers remain portable; VM execution internals become
// intentionally unreachable when this target cannot run the VM backend.
#![cfg_attr(
    not(any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    )),
    allow(dead_code, unused_imports)
)]

/// Agent Trajectory Interchange Format projection and wire types.
pub mod atif;
mod capture_proxy;
mod codex;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
/// Matched Nanocodex-versus-Codex execution and retained comparison reports.
pub mod differential;
mod digest;
mod evaluator;
mod event;
mod job;
mod native;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
/// Closed, declarative evaluation profiles over native task packages.
pub mod profile;
mod result;
mod task;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
pub mod vm;
/// Durable SQLite ledger for agent-selected evaluation coordinates.
pub mod workset;

pub(crate) use atif::{
    AtifAgent, AtifAgentExtra, AtifBuilder, AtifObservation, AtifObservationExtra,
    AtifObservationResult, AtifSource, AtifStep, AtifToolCall, AtifToolCallExtra, AtifTrajectory,
};
pub(crate) use capture_proxy::{
    ResponsesCaptureProxy, ResponsesCaptureProxyConfig, ResponsesModelCatalogOverride,
};
pub(crate) use codex::{
    CodexCommandOutput, CodexCommandRunner, CodexCommandRunnerError, CodexCommandStatus, CodexExec,
    CodexExecError, project_codex_atif,
};
pub use evaluator::{EvalError, EvalRun, Evaluator, EvaluatorBuilder};
pub use event::{
    EvalEvent, EvalEventAttempt, EvalEventKind, EvalEventStream, EvalEventStreamError, EvalEvents,
};
pub use result::{
    AgentMetadata, AgentResult, AgentStatus, BillingCompleteness, CleanupDiagnostic, CleanupPhase,
    CleanupStatus, EvalArtifacts, EvalAttemptOutcome, EvalCleanup, EvalEnvironment, EvalException,
    EvalExceptionKind, EvalFailure, EvalFailureTiming, EvalOutcome, EvalResult, EvalStatus,
    EvalTiming, MeasurementCompleteness, PhaseTiming, UsageTotals, VerifierResult,
};
pub use task::{
    NetworkPolicy, OciImage, Resources, Task, TaskLoadError, Verifier, VerifierCollect,
    VerifierEnvironmentMode,
};
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
pub use vm::{CachePolicy, VmResources, VmResourcesBuilder, VmResourcesError};
