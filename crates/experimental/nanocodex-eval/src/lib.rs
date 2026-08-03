//! Typed, VM-isolated evaluation for Nanocodex agents.
//!
//! This crate owns task loading, bounded scheduling, resumable jobs, typed
//! events and outcomes, and task × agent × trial sweeps. Every benchmark
//! attempt executes its tools and verifier in a prepared microVM.
//!
//! # Run a sweep
//!
//! ```no_run
//! use nanocodex_agent::{Nanocodex, OpenAi, Thinking};
//! use nanocodex_eval::{Evaluator, Sweep, Task, VmResources};
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
//! let sweep = Sweep::builder()
//!     .task(task)
//!     .agent("gpt-5.6-sol-medium", agent.clone())?
//!     .trials(5)
//!     .build()?;
//!
//! let evaluator = Evaluator::builder(agent, backend)
//!     .output_directory(".nanocodex/evals")
//!     .max_concurrency(4)
//!     .max_memory_mb(16_384)
//!     .resume_incomplete(sweep)
//!     .build()?;
//! let run = evaluator.sweep();
//! let mut stream = run.events().subscribe();
//! let event_task = tokio::spawn(async move {
//!     while let Some(event) = stream.recv().await? {
//!         println!("{} {:?}", event.sequence, event.kind);
//!     }
//!     Ok::<_, nanocodex_eval::EvalEventStreamError>(())
//! });
//!
//! let results = run.await?;
//! println!("{} attempts, {} skipped", results.attempts().len(), results.skipped());
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

/// Aggregated metrics derived from retained evaluator outcomes.
pub mod aggregate;
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
mod durable;
mod evaluator;
mod event;
pub mod harbor;
mod job;
mod native;
mod result;
mod sweep;
mod task;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
pub mod vm;

pub(crate) use aggregate::{
    AggregateDataset, AttemptBuildIdentity, AttemptConfigurationIdentity, AttemptFact,
    AttemptFactArtifacts, AttemptRuntimeMetrics, AttemptTaskIdentity, AttemptUsage,
    AttemptVerifierFact, AttemptVerifierIdentity, LatencyBreakdown,
};
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
    EvalTiming, MeasurementCompleteness, PhaseTiming, SweepAttemptResult, SweepResults,
    UsageTotals, VerifierResult,
};
pub use sweep::{AgentId, AgentIdError, Sweep, SweepBuilder, SweepError};
pub use task::{
    NetworkPolicy, OciImage, Resources, Task, TaskLoadError, Verifier, VerifierCollect,
    VerifierEnvironmentMode,
};
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
pub use vm::{CachePolicy, VmResources, VmResourcesBuilder, VmResourcesError};
