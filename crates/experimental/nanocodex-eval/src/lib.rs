//! Typed, VM-isolated evaluation for Nanocodex agents.
//!
//! This crate owns task loading, durable SQLite worksets, typed events and
//! outcomes, and VM-isolated execution. Applications choose one exact workset
//! coordinate family; SQLite allocates its internal repetition and fences the
//! accepted completion.
//!
//! # Open a durable workset
//!
//! ```no_run
//! use std::time::Duration;
//! use nanocodex_eval::{Evaluation, EvaluationClaim, EvaluationSelector};
//!
//! # async fn evaluate() -> Result<(), Box<dyn std::error::Error>> {
//! let evaluation = Evaluation::open(
//!     "nanocodex.toml",
//!     "local-smoke",
//!     ".nanocodex/evals",
//! )?;
//! let selector = EvaluationSelector::new("tasks/write-greeting");
//! match evaluation.claim(&selector, Duration::from_secs(300))? {
//!     EvaluationClaim::Prepare(claim) => {
//!         // Prepare the immutable package exposed by `claim.task()`.
//!         claim.complete()?;
//!     }
//!     EvaluationClaim::Run(claim) => {
//!         // Execute exactly this SQLite treatment and retain its evidence.
//!         let evidence = claim.output_directory().to_path_buf();
//!         claim.complete(&evidence)?;
//!     }
//!     EvaluationClaim::Busy(_) | EvaluationClaim::Complete => {}
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Claims renew their own lease and expose only fenced completion or retry.

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
pub mod coordinator;
mod digest;
mod evaluation;
mod evaluator;
mod event;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
/// Configured external harness execution inside evaluator-owned sandboxes.
pub mod harness;
mod harness_exec;
mod job;
mod native;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
mod profile;
mod result;
mod task;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
pub mod vm;
mod workset;

pub(crate) use atif::{
    AtifAgent, AtifAgentExtra, AtifObservation, AtifObservationExtra, AtifObservationResult,
    AtifSource, AtifStep, AtifToolCall, AtifToolCallExtra, AtifTrajectory,
};
pub(crate) use capture_proxy::{ResponsesCaptureProxy, ResponsesCaptureProxyConfig};
pub use evaluation::{
    CoordinateClaim, Evaluation, EvaluationBusy, EvaluationClaim, EvaluationCounts,
    EvaluationError, EvaluationFamilyStatus, EvaluationSelector, EvaluationStatus,
    EvaluationTreatment, EvaluationWork, PreparationClaim,
};
pub use evaluator::{EvalError, EvalRun, Evaluator, EvaluatorBuilder};
pub use event::{
    EvalEvent, EvalEventAttempt, EvalEventKind, EvalEventStream, EvalEventStreamError, EvalEvents,
};
pub(crate) use harness_exec::{
    HarnessCommandOutput, HarnessCommandRunner, HarnessCommandRunnerError, HarnessCommandStatus,
    HarnessExec, HarnessExecError,
};
pub use profile::ResolvedHarness;
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
