//! Harbor-compatible artifacts for [`crate`].
//!
//! The evaluator's typed result is authoritative. This module subscribes to its
//! optional event stream and durably projects each attempt into Harbor job,
//! trial, trajectory, verifier, and ATIF files.
//!
//! # Record a job
//!
//! ```no_run
//! use nanocodex_agent::{Nanocodex, OpenAi};
//! use nanocodex_eval::{Evaluator, Task, VmResources, harbor::Harbor};
//!
//! # async fn evaluate() -> Result<(), Box<dyn std::error::Error>> {
//! let agent = Nanocodex::builder(OpenAi::new(std::env::var("OPENAI_API_KEY")?)?).instructions(
//!     "Work in the provided workspace, complete the task, and verify it.",
//! );
//! let task = Task::load("tasks/write-greeting")?;
//! let resources = VmResources::builder("nanocodex", "runtime.ext4")
//!     .task(task.clone())
//!     .prepare()
//!     .await?;
//! let evaluator = Evaluator::builder(agent, resources.backend().await?)
//!     .output_directory(".nanocodex/evals")
//!     .build()?;
//! let run = evaluator.task(task);
//! let recorder = Harbor::new(&evaluator)?.record(run.events().subscribe())?;
//! let result = run.await?;
//! let job = recorder.finish(vec![result]).await?;
//! println!("{}", job.directory().display());
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

mod checksum;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::{
    AgentMetadata, AggregateDataset, AtifBuilder, AtifTrajectory, AttemptBuildIdentity,
    AttemptConfigurationIdentity, AttemptFact, AttemptFactArtifacts, AttemptRuntimeMetrics,
    AttemptTaskIdentity, AttemptUsage, AttemptVerifierFact, AttemptVerifierIdentity,
    BillingCompleteness, EvalAttemptOutcome, EvalCleanup, EvalEnvironment, EvalEventKind,
    EvalEventStream, EvalEventStreamError, EvalExceptionKind, EvalFailure, EvalOutcome, EvalResult,
    Evaluator, LatencyBreakdown, MeasurementCompleteness, PhaseTiming, Task, TaskLoadError,
    UsageTotals,
    digest::PACKAGE_DIGEST_SCHEMA,
    durable::scan_manifest_trials,
    sweep::{RunCoordinate, RunManifest},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{sync::oneshot, task::JoinHandle};
use url::Url;
use uuid::Uuid;

use checksum::{directory_hash, packager_content_hash};

#[derive(Debug, thiserror::Error)]
/// An error produced while recording or publishing Harbor-compatible artifacts.
pub enum HarborError {
    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A Harbor JSON document could not be encoded or decoded.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// A task package changed or became unreadable before artifact projection.
    #[error(transparent)]
    TaskPackage(#[from] TaskLoadError),

    /// A task directory contained no packageable files.
    #[error("task directory is empty: {0}")]
    EmptyTask(PathBuf),

    /// Harbor's gitignore-compatible task packaging rules could not be parsed.
    #[error(transparent)]
    PackageIgnore(#[from] ignore::Error),

    /// Following a symbolic link would make task packaging cyclic.
    #[error("task directory contains a cyclic symbolic link: {0}")]
    CyclicTaskDirectory(PathBuf),

    /// A trial path could not be represented as a `file:` URL.
    #[error("trial directory cannot be represented as a file URL: {0}")]
    InvalidTrialPath(PathBuf),

    /// A retained terminal result did not belong to its finite run.
    #[error("invalid durable evaluation trial: {0}")]
    InvalidDurableTrial(String),

    /// A terminal result was about to be committed before one of its artifacts.
    #[error("terminal evaluation result prerequisite is missing: {0}")]
    MissingTerminalPrerequisite(PathBuf),

    /// The evaluator event subscription lagged or otherwise failed.
    #[error(transparent)]
    EventStream(#[from] EvalEventStreamError),

    /// An event referenced an attempt before its start event.
    #[error("received events for attempt {0} before attempt.started")]
    MissingAttempt(Uuid),

    /// An attempt-level event omitted its attempt identity.
    #[error("attempt-level evaluator event omitted attempt identity")]
    MissingAttemptIdentity,

    /// More than one start event was received for an attempt.
    #[error("received duplicate attempt.started for attempt {0}")]
    DuplicateAttempt(Uuid),

    /// More than one terminal event was received for an attempt.
    #[error("received duplicate terminal event for attempt {0}")]
    DuplicateTerminal(Uuid),

    /// The recorder task stopped before it could be finalized.
    #[error("Harbor recorder stopped before finish")]
    RecorderStopped,

    /// The evaluator event stream ended before the requested batch completed.
    #[error("Evaluator event stream closed before Harbor recording finished")]
    EventStreamClosed,

    /// The background recorder task failed.
    #[error("Harbor recorder task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// Recording was started without an active Tokio runtime.
    #[error("Harbor recording requires an active Tokio runtime: {0}")]
    Runtime(#[from] tokio::runtime::TryCurrentError),
}

/// Explicit Harbor compatibility adapter for one evaluation job.
pub struct Harbor {
    artifacts: HarborArtifacts,
}

/// Active, streaming Harbor projection of an independent event subscription.
pub struct HarborRecorder {
    finish: Option<oneshot::Sender<FinishRequest>>,
    task: Option<JoinHandle<Result<HarborJob, HarborError>>>,
}

#[derive(Clone, Debug)]
/// A durably committed Harbor-compatible evaluation job.
pub struct HarborJob {
    id: Uuid,
    directory: PathBuf,
}

impl Harbor {
    /// Validates that a task is stable and readable under every task identity
    /// algorithm emitted by the Harbor adapter.
    ///
    /// This performs Nanocodex package validation before and after computing
    /// Harbor's trial checksum and publisher content hash, matching the
    /// complete identity-read stack used while committing a terminal trial.
    ///
    /// # Errors
    ///
    /// Returns an error when the task changed since loading, cannot be read,
    /// or cannot be hashed using Harbor's compatibility rules.
    pub fn validate_task_package(task: &Task) -> Result<(), HarborError> {
        let _ = validated_task_identity(task)?;
        task.validate_package()?;
        Ok(())
    }

    /// Attaches the adapter to a reusable evaluator and its artifact directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the evaluator directory cannot be initialized with
    /// Harbor job metadata.
    pub fn new(eval: &Evaluator) -> Result<Self, HarborError> {
        Ok(Self {
            artifacts: HarborArtifacts::attach(eval)?,
        })
    }

    /// Starts consuming one independent event subscription immediately.
    ///
    /// # Errors
    ///
    /// Returns an error when called without an active Tokio runtime.
    pub fn record(self, events: EvalEventStream) -> Result<HarborRecorder, HarborError> {
        let (finish, finish_receiver) = oneshot::channel();
        let task = tokio::runtime::Handle::try_current()?.spawn(record(
            self.artifacts,
            events,
            finish_receiver,
        ));
        Ok(HarborRecorder {
            finish: Some(finish),
            task: Some(task),
        })
    }
}

impl HarborRecorder {
    /// Waits until every supplied outcome's terminal event has been recorded,
    /// then commits the final Harbor job result.
    ///
    /// # Errors
    ///
    /// Returns an error on event lag, malformed event payloads, filesystem
    /// failures, or premature recorder termination.
    pub async fn finish(
        mut self,
        outcomes: Vec<EvalAttemptOutcome>,
    ) -> Result<HarborJob, HarborError> {
        self.finish_request(FinishRequest::Outcomes(outcomes)).await
    }

    /// Finishes after the requested number of completed or errored attempts.
    ///
    /// This is the batch boundary used when individual attempts may fail while
    /// the evaluator continues running unrelated work.
    ///
    /// # Errors
    ///
    /// Returns an error on event lag, malformed event payloads, filesystem
    /// failures, premature recorder termination, or a mismatched attempt count.
    pub async fn finish_all(mut self, attempts: usize) -> Result<HarborJob, HarborError> {
        self.finish_request(FinishRequest::TerminalCount(attempts))
            .await
    }

    async fn finish_request(&mut self, request: FinishRequest) -> Result<HarborJob, HarborError> {
        let finish = self.finish.take().ok_or(HarborError::RecorderStopped)?;
        let task = self.task.take().ok_or(HarborError::RecorderStopped)?;
        // A closed finish receiver means the recorder already stopped. Its
        // retained task result carries the specific projection failure.
        drop(finish.send(request));
        task.await?
    }
}

impl Drop for HarborRecorder {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl HarborJob {
    /// Returns the stable job identifier.
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the directory containing the committed Harbor artifacts.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Reconstructs the stable aggregate dataset from every durable trial.
    ///
    /// This includes trials completed by an earlier process and skipped when a
    /// finite run resumes.
    ///
    /// # Errors
    ///
    /// Returns an error when retained JSON cannot be read or a retained sweep
    /// trial does not match its original run coordinate.
    pub fn aggregate_dataset(&self) -> Result<AggregateDataset, HarborError> {
        let manifest =
            HarborArtifacts::read_json_if_exists::<RunManifest>(&self.directory.join("run.json"))?;
        let mut attempts =
            HarborArtifacts::durable_trials(&self.directory, self.id, manifest.as_ref())?
                .into_iter()
                .map(|trial| {
                    let (configuration, repetition) = match trial.coordinate.as_ref() {
                        Some(coordinate) => (
                            coordinate.agent().as_str().to_owned(),
                            coordinate.repetition(),
                        ),
                        None => ("default".to_owned(), 1),
                    };
                    Ok(trial.attempt_fact(configuration, repetition))
                })
                .collect::<Result<Vec<_>, HarborError>>()?;
        attempts.sort_by(|left, right| {
            (
                left.task.name.as_str(),
                left.configuration.id.as_str(),
                left.repetition,
                left.attempt_id,
            )
                .cmp(&(
                    right.task.name.as_str(),
                    right.configuration.id.as_str(),
                    right.repetition,
                    right.attempt_id,
                ))
        });
        Ok(AggregateDataset::new(attempts))
    }
}

struct AttemptRecording {
    events: BufWriter<File>,
    atif: AtifBuilder,
}

enum FinishRequest {
    Outcomes(Vec<EvalAttemptOutcome>),
    TerminalCount(usize),
}

fn finished_attempt_count(
    request: Option<&FinishRequest>,
    completed: &HashSet<Uuid>,
) -> Option<usize> {
    match request? {
        FinishRequest::Outcomes(outcomes)
            if outcomes
                .iter()
                .all(|outcome| completed.contains(&outcome.attempt_id())) =>
        {
            Some(outcomes.len())
        }
        FinishRequest::TerminalCount(expected) if completed.len() == *expected => Some(*expected),
        FinishRequest::Outcomes(_) | FinishRequest::TerminalCount(_) => None,
    }
}

async fn record(
    artifacts: HarborArtifacts,
    mut events: EvalEventStream,
    mut finish: oneshot::Receiver<FinishRequest>,
) -> Result<HarborJob, HarborError> {
    let mut attempts = HashMap::<Uuid, AttemptRecording>::new();
    let mut seen = HashSet::<Uuid>::new();
    let mut completed = HashSet::<Uuid>::new();
    let mut finish_request = None::<FinishRequest>;

    loop {
        if let Some(n_total_trials) = finished_attempt_count(finish_request.as_ref(), &completed) {
            artifacts.write_job(n_total_trials, attempts.len())?;
            return Ok(HarborJob {
                id: artifacts.job_id,
                directory: artifacts.root.clone(),
            });
        }

        tokio::select! {
            requested = &mut finish, if finish_request.is_none() => {
                finish_request = Some(requested.map_err(|_| HarborError::RecorderStopped)?);
            }
            event = events.recv() => {
                let Some(event) = event? else {
                    if finish_request.is_none() {
                        finish_request = Some(
                            (&mut finish)
                                .await
                                .map_err(|_| HarborError::RecorderStopped)?,
                        );
                    }
                    if finished_attempt_count(finish_request.as_ref(), &completed).is_none() {
                        return Err(HarborError::EventStreamClosed);
                    }
                    continue;
                };
                match &event.kind {
                    EvalEventKind::AttemptStarted { prompt, .. } => {
                        let attempt = event.attempt.as_ref().ok_or(HarborError::MissingAttemptIdentity)?;
                        if !seen.insert(attempt.id) {
                            return Err(HarborError::DuplicateAttempt(attempt.id));
                        }
                        let writer = artifacts.write_input(
                            attempt.id,
                            &attempt.trial_name,
                            prompt,
                        )?;
                        attempts.insert(attempt.id, AttemptRecording {
                            events: writer,
                            atif: AtifBuilder::default(),
                        });
                        artifacts.write_job(completed.len(), attempts.len())?;
                    }
                    EvalEventKind::Agent(agent_event) => {
                        let identity = event.attempt.as_ref().ok_or(HarborError::MissingAttemptIdentity)?;
                        let attempt = attempts
                            .get_mut(&identity.id)
                            .ok_or(HarborError::MissingAttempt(identity.id))?;
                        serde_json::to_writer(&mut attempt.events, agent_event)?;
                        attempt.events.write_all(b"\n")?;
                        attempt.events.flush()?;
                        attempt.atif.apply(agent_event)?;
                    }
                    EvalEventKind::Completed(result) => {
                        let identity = event.attempt.as_ref().ok_or(HarborError::MissingAttemptIdentity)?;
                        if completed.contains(&identity.id) {
                            return Err(HarborError::DuplicateTerminal(identity.id));
                        }
                        let mut attempt = attempts
                            .remove(&identity.id)
                            .ok_or(HarborError::MissingAttempt(identity.id))?;
                        attempt.events.flush()?;
                        attempt.events.get_ref().sync_all()?;
                        let result = result.as_ref().clone();
                        let trajectory = match &result.agent {
                            Some(agent) => attempt.atif.finish(result.task(), agent),
                            None => attempt.atif.finish_failure(result.task()),
                        };
                        artifacts.write_trial(&result, &trajectory)?;
                        completed.insert(result.attempt_id);
                        artifacts.write_job(completed.len(), attempts.len())?;
                    }
                    EvalEventKind::Failed(failure) => {
                        let identity = event.attempt.as_ref().ok_or(HarborError::MissingAttemptIdentity)?;
                        if completed.contains(&identity.id) {
                            return Err(HarborError::DuplicateTerminal(identity.id));
                        }
                        seen.insert(identity.id);
                        let trajectory = if let Some(mut attempt) = attempts.remove(&identity.id) {
                            attempt.events.flush()?;
                            attempt.events.get_ref().sync_all()?;
                            match failure.agent.as_ref() {
                                Some(agent) => attempt.atif.finish(failure.task(), agent),
                                None => attempt.atif.finish_failure(failure.task()),
                            }
                        } else {
                            let mut events = artifacts.write_input(
                                identity.id,
                                &identity.trial_name,
                                failure.task().prompt(),
                            )?;
                            events.flush()?;
                            events.get_ref().sync_all()?;
                            match failure.agent.as_ref() {
                                Some(agent) => {
                                    AtifBuilder::default().finish(failure.task(), agent)
                                }
                                None => AtifBuilder::default().finish_failure(failure.task()),
                            }
                        };
                        let failure = failure.as_ref().clone();
                        artifacts.write_failure(&failure, &trajectory)?;
                        completed.insert(failure.attempt_id);
                        artifacts.write_job(completed.len(), attempts.len())?;
                    }
                    EvalEventKind::VerifierStarted
                    | EvalEventKind::VerifierOutput { .. }
                    | EvalEventKind::VerifierCompleted(_)
                    | EvalEventKind::RunCompleted { .. }
                    | EvalEventKind::RunFailed { .. } => {}
                }
            }
        }
    }
}

struct HarborArtifacts {
    job_id: Uuid,
    started_at: DateTime<Utc>,
    root: PathBuf,
    jobs_dir: PathBuf,
    max_concurrency: usize,
    environment: EvalEnvironment,
    planned_attempts: Option<usize>,
    manifest: Option<RunManifest>,
    baseline: Option<HarborJobResult>,
    recorded_trials: Mutex<Vec<HarborRecordedTrial>>,
}

fn validated_task_identity(task: &Task) -> Result<(String, String), HarborError> {
    task.validate_package()?;
    Ok((
        directory_hash(task.root())?,
        packager_content_hash(task.root())?,
    ))
}

impl HarborArtifacts {
    fn attach(eval: &Evaluator) -> Result<Self, HarborError> {
        let root = eval.directory().to_path_buf();
        let manifest = Self::read_json_if_exists::<RunManifest>(&root.join("run.json"))?;
        let baseline = Self::read_json_if_exists(&root.join("result.json"))?;
        let recorded_trials = Self::recorded_trials(&root, eval.id(), manifest.as_ref())?;
        let artifacts = Self {
            job_id: eval.id(),
            started_at: eval.started_at(),
            root,
            jobs_dir: eval.parent_directory().to_path_buf(),
            max_concurrency: eval.max_concurrency(),
            environment: eval.attempt_environment(),
            planned_attempts: eval.planned_attempts(),
            manifest,
            baseline,
            recorded_trials: Mutex::new(recorded_trials),
        };
        Self::write_file(&artifacts.root.join("job.log"), [])?;
        artifacts.write_job_metadata()?;
        artifacts.write_job(artifacts.planned_attempts.unwrap_or(0), 0)?;
        Ok(artifacts)
    }

    fn recorded_trials(
        root: &Path,
        job_id: Uuid,
        manifest: Option<&RunManifest>,
    ) -> Result<Vec<HarborRecordedTrial>, HarborError> {
        let mut recorded = Vec::new();
        for trial in Self::durable_result_paths(root, job_id, manifest)? {
            let Some(lock) =
                Self::read_json_if_exists::<HarborTrialLock>(&trial.directory.join("lock.json"))?
            else {
                return Err(HarborError::InvalidDurableTrial(format!(
                    "durable trial {} has no per-trial lock",
                    trial.directory.display()
                )));
            };
            recorded.push(Self::recorded_trial(lock));
        }
        Ok(recorded)
    }

    fn recorded_trial(lock: HarborTrialLock) -> HarborRecordedTrial {
        HarborRecordedTrial {
            task: HarborTaskConfig {
                path: lock.task.path.clone(),
                source: lock.task.source.clone(),
            },
            agent: lock.agent.clone(),
            lock,
        }
    }

    fn write_input(
        &self,
        attempt_id: Uuid,
        trial_name: &str,
        prompt: &str,
    ) -> Result<BufWriter<File>, HarborError> {
        let root = self.root.join(trial_name);
        let agent = root.join("agent");
        fs::create_dir_all(&agent)?;
        let input = HarborInput {
            protocol_version: 1,
            request_id: Some(attempt_id.to_string()),
            kind: "input",
            payload: HarborInputPayload {
                instruction: prompt,
            },
        };
        let mut bytes = serde_json::to_vec(&input)?;
        bytes.push(b'\n');
        Self::write_file(&agent.join("input.jsonl"), bytes)?;
        Ok(BufWriter::new(File::create(agent.join("events.jsonl"))?))
    }

    fn write_trial(
        &self,
        result: &EvalResult,
        trajectory: &AtifTrajectory,
    ) -> Result<(), HarborError> {
        let task = result.task();
        let (task_checksum, task_content_hash) = validated_task_identity(task)?;
        let root = &result.artifacts.directory;
        let agent = root.join("agent");
        let input_path = agent.join("input.jsonl");
        let events_path = agent.join("events.jsonl");
        let trajectory_path = agent.join("trajectory.json");
        let stderr_path = agent.join("stderr.log");
        let config_path = root.join("config.json");
        let manifest_path = root.join("artifacts/manifest.json");
        let trial_log_path = root.join("trial.log");
        let lock_path = root.join("lock.json");
        let result_path = root.join("result.json");
        let task_path = task.root().to_path_buf();
        let task_digest = task.content_digest();
        let model = result
            .agent
            .as_ref()
            .map_or(trajectory.agent.model_name.as_str(), |agent| {
                agent.model.as_str()
            });
        let effort = result
            .agent
            .as_ref()
            .map(|agent| agent.effort.as_str())
            .or_else(|| {
                trajectory
                    .steps
                    .iter()
                    .find_map(|step| step.reasoning_effort.as_deref())
            })
            .unwrap_or("unknown");
        let config = HarborTrialConfig {
            task: HarborTaskConfig {
                path: task_path.clone(),
                source: Some("nanocodex/local".to_owned()),
            },
            trial_name: &result.trial_name,
            trials_dir: &self.root,
            agent: harbor_agent_config(model, effort),
            environment: HarborEnvironmentConfig::from(result.environment),
            verifier: HarborVerifierConfig::native(),
            artifacts: Vec::new(),
            extra_instruction_paths: Vec::new(),
            job_id: self.job_id,
        };
        Self::write_json(&config_path, &config)?;
        Self::write_json(&trajectory_path, trajectory)?;
        Self::write_json(&manifest_path, &Vec::<HarborArtifactManifestEntry>::new())?;

        let trial_uri = Url::from_directory_path(root)
            .map_err(|()| HarborError::InvalidTrialPath(root.clone()))?
            .to_string();
        let trial_result = HarborTrialResult {
            id: result.attempt_id,
            task_name: &result.task_name,
            trial_name: &result.trial_name,
            trial_uri,
            task_id: HarborTaskId { path: task_path },
            source: "nanocodex/local",
            task_checksum,
            outcome: result.outcome,
            scored: true,
            cleanup: &result.cleanup,
            config,
            agent_info: HarborAgentInfo {
                name: "nanocodex",
                version: env!("CARGO_PKG_VERSION"),
                model_info: HarborModelInfo {
                    name: model,
                    provider: "openai",
                },
            },
            agent_result: result.agent.as_ref().map(|agent| HarborAgentResult {
                n_input_tokens: agent.usage.input_tokens,
                n_cache_tokens: agent.usage.cached_input_tokens,
                n_output_tokens: agent.usage.output_tokens,
                cost_usd: agent.cost_usd,
                billing_completeness: agent.billing_completeness,
                rollout_details: None,
                metadata: &agent.metadata,
            }),
            verifier_result: Some(HarborVerifierResult {
                exit_code: result.verifier.exit_code,
                rewards: &result.verifier.rewards,
            }),
            started_at: result.timing.started_at,
            finished_at: result.timing.finished_at,
            queue_wait: Some(&result.timing.queue_wait),
            environment_setup: Some(&result.timing.environment_setup),
            environment_readiness: Some(&result.timing.environment_readiness),
            agent_setup: Some(&result.timing.agent_setup),
            agent_execution: Some(&result.timing.agent_execution),
            verifier: Some(&result.timing.verifier),
            exception_info: result
                .exception
                .as_ref()
                .map(|exception| HarborExceptionInfo {
                    exception_type: exception.kind.harbor_exception_type(),
                    exception_message: &exception.message,
                    exception_traceback: &exception.traceback,
                    occurred_at: exception.occurred_at,
                }),
            step_results: None,
        };
        let exception_log = result
            .exception
            .as_ref()
            .map_or(&[][..], |exception| exception.traceback.as_bytes());
        Self::write_file(&trial_log_path, exception_log)?;
        Self::write_file(&stderr_path, exception_log)?;

        let lock = HarborTrialLock::new(
            task,
            model,
            effort,
            &task_content_hash,
            task_digest,
            result.environment,
        );
        Self::write_json(&lock_path, &lock)?;
        task.validate_package()?;
        Self::write_terminal_json(
            &result_path,
            &trial_result,
            &[
                input_path.as_path(),
                events_path.as_path(),
                config_path.as_path(),
                trajectory_path.as_path(),
                manifest_path.as_path(),
                trial_log_path.as_path(),
                stderr_path.as_path(),
                result.artifacts.verifier_output.as_path(),
                lock_path.as_path(),
            ],
            &self.root,
        )?;
        {
            let mut recorded = self
                .recorded_trials
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recorded.push(HarborRecordedTrial {
                task: HarborTaskConfig {
                    path: task.root().to_path_buf(),
                    source: Some("nanocodex/local".to_owned()),
                },
                agent: harbor_agent_config(model, effort),
                lock,
            });
        }
        self.write_job_metadata()
    }

    fn write_failure(
        &self,
        failure: &EvalFailure,
        trajectory: &AtifTrajectory,
    ) -> Result<(), HarborError> {
        let task = failure.task();
        let (task_checksum, task_content_hash) = validated_task_identity(task)?;
        let root = &failure.artifacts.directory;
        let agent = root.join("agent");
        let input_path = agent.join("input.jsonl");
        let events_path = agent.join("events.jsonl");
        let trajectory_path = agent.join("trajectory.json");
        let stderr_path = agent.join("stderr.log");
        let config_path = root.join("config.json");
        let manifest_path = root.join("artifacts/manifest.json");
        let trial_log_path = root.join("trial.log");
        let lock_path = root.join("lock.json");
        let result_path = root.join("result.json");
        let task_path = task.root().to_path_buf();
        let task_digest = task.content_digest();
        let model = trajectory.agent.model_name.as_str();
        let effort = trajectory
            .steps
            .iter()
            .find_map(|step| step.reasoning_effort.as_deref())
            .unwrap_or(&failure.effort);
        let config = HarborTrialConfig {
            task: HarborTaskConfig {
                path: task_path.clone(),
                source: Some("nanocodex/local".to_owned()),
            },
            trial_name: &failure.trial_name,
            trials_dir: &self.root,
            agent: harbor_agent_config(model, effort),
            environment: HarborEnvironmentConfig::from(failure.environment),
            verifier: HarborVerifierConfig::native(),
            artifacts: Vec::new(),
            extra_instruction_paths: Vec::new(),
            job_id: self.job_id,
        };
        Self::write_json(&config_path, &config)?;
        Self::write_json(&trajectory_path, trajectory)?;
        Self::write_json(&manifest_path, &Vec::<HarborArtifactManifestEntry>::new())?;

        let trial_uri = Url::from_directory_path(root)
            .map_err(|()| HarborError::InvalidTrialPath(root.clone()))?
            .to_string();
        let trial_result = HarborTrialResult {
            id: failure.attempt_id,
            task_name: &failure.task_name,
            trial_name: &failure.trial_name,
            trial_uri,
            task_id: HarborTaskId { path: task_path },
            source: "nanocodex/local",
            task_checksum,
            outcome: failure.exception.outcome,
            scored: false,
            cleanup: &failure.cleanup,
            config,
            agent_info: HarborAgentInfo {
                name: "nanocodex",
                version: env!("CARGO_PKG_VERSION"),
                model_info: HarborModelInfo {
                    name: model,
                    provider: "openai",
                },
            },
            agent_result: failure.agent.as_ref().map(|agent| HarborAgentResult {
                n_input_tokens: agent.usage.input_tokens,
                n_cache_tokens: agent.usage.cached_input_tokens,
                n_output_tokens: agent.usage.output_tokens,
                cost_usd: agent.cost_usd,
                billing_completeness: agent.billing_completeness,
                rollout_details: None,
                metadata: &agent.metadata,
            }),
            verifier_result: failure
                .verifier
                .as_ref()
                .map(|verifier| HarborVerifierResult {
                    exit_code: verifier.exit_code,
                    rewards: &verifier.rewards,
                }),
            started_at: failure.started_at,
            finished_at: failure.finished_at,
            queue_wait: Some(&failure.timing.queue_wait),
            environment_setup: failure.timing.environment_setup.as_ref(),
            environment_readiness: failure.timing.environment_readiness.as_ref(),
            agent_setup: failure.timing.agent_setup.as_ref(),
            agent_execution: failure.timing.agent_execution.as_ref(),
            verifier: failure.timing.verifier.as_ref(),
            exception_info: Some(HarborExceptionInfo {
                exception_type: failure.exception.kind.harbor_exception_type(),
                exception_message: &failure.exception.message,
                exception_traceback: &failure.exception.traceback,
                occurred_at: failure.exception.occurred_at,
            }),
            step_results: None,
        };
        Self::write_file(&trial_log_path, failure.exception.traceback.as_bytes())?;
        Self::write_file(&stderr_path, failure.exception.traceback.as_bytes())?;

        let lock = HarborTrialLock::new(
            task,
            model,
            effort,
            &task_content_hash,
            task_digest,
            failure.environment,
        );
        Self::write_json(&lock_path, &lock)?;
        task.validate_package()?;
        Self::write_terminal_json(
            &result_path,
            &trial_result,
            &[
                input_path.as_path(),
                events_path.as_path(),
                config_path.as_path(),
                trajectory_path.as_path(),
                manifest_path.as_path(),
                trial_log_path.as_path(),
                stderr_path.as_path(),
                lock_path.as_path(),
            ],
            &self.root,
        )?;
        {
            let mut recorded = self
                .recorded_trials
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recorded.push(HarborRecordedTrial {
                task: HarborTaskConfig {
                    path: task.root().to_path_buf(),
                    source: Some("nanocodex/local".to_owned()),
                },
                agent: harbor_agent_config(model, effort),
                lock,
            });
        }
        self.write_job_metadata()
    }

    fn durable_result_paths(
        root: &Path,
        job_id: Uuid,
        manifest: Option<&RunManifest>,
    ) -> Result<Vec<DurableResultPath>, HarborError> {
        if let Some(manifest) = manifest {
            return scan_manifest_trials(root, job_id, manifest)
                .map(|trials| {
                    trials
                        .into_iter()
                        .map(|trial| DurableResultPath {
                            directory: trial.directory().to_path_buf(),
                            result_path: trial.result_path().to_path_buf(),
                            coordinate: Some(trial.coordinate().clone()),
                        })
                        .collect()
                })
                .map_err(|error| HarborError::InvalidDurableTrial(error.to_string()));
        }

        let mut trials = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let result_path = entry.path().join("result.json");
                if result_path.is_file() {
                    trials.push(DurableResultPath {
                        directory: entry.path(),
                        result_path,
                        coordinate: None,
                    });
                }
            }
        }
        trials.sort_unstable_by(|left, right| left.result_path.cmp(&right.result_path));
        Ok(trials)
    }

    fn durable_trials(
        root: &Path,
        job_id: Uuid,
        manifest: Option<&RunManifest>,
    ) -> Result<Vec<DurableHarborTrial>, HarborError> {
        Self::durable_result_paths(root, job_id, manifest)?
            .into_iter()
            .map(|trial| {
                let result = serde_json::from_slice(&fs::read(&trial.result_path)?)?;
                let lock = serde_json::from_slice(&fs::read(trial.directory.join("lock.json"))?)?;
                Ok(DurableHarborTrial {
                    directory: trial.directory,
                    result,
                    coordinate: trial.coordinate,
                    lock,
                })
            })
            .collect()
    }

    fn write_job(&self, n_total_trials: usize, n_running_trials: usize) -> Result<(), HarborError> {
        let now = Utc::now();
        let trials = Self::durable_trials(&self.root, self.job_id, self.manifest.as_ref())?;
        let mut stats = HarborJobStats::from_trials(&trials);
        for (eval_key, pass_at_k) in compute_harbor_pass_at_k(&trials) {
            stats.evals.entry(eval_key).or_default().pass_at_k = pass_at_k;
        }
        let baseline_total = if self.manifest.is_some() {
            0
        } else {
            self.baseline
                .as_ref()
                .map_or(0, |baseline| baseline.n_total_trials)
        };
        let n_total_trials = n_total_trials
            .max(self.planned_attempts.unwrap_or(0))
            .max(baseline_total)
            .max(stats.n_completed_trials.saturating_add(n_running_trials));
        stats.n_running_trials = n_running_trials;
        stats.n_pending_trials = n_total_trials
            .saturating_sub(stats.n_completed_trials)
            .saturating_sub(stats.n_running_trials);
        let job = HarborJobResult {
            id: self.job_id,
            started_at: self
                .baseline
                .as_ref()
                .map_or(self.started_at, |baseline| baseline.started_at),
            updated_at: now,
            finished_at: (n_total_trials > 0 && stats.n_completed_trials == n_total_trials)
                .then_some(now),
            n_total_trials,
            stats,
        };
        Self::write_json(&self.root.join("result.json"), &job)
    }

    fn write_job_metadata(&self) -> Result<(), HarborError> {
        let recorded = self
            .recorded_trials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut tasks = Vec::new();
        let mut agents = Vec::new();
        for trial in recorded.iter() {
            if !tasks
                .iter()
                .any(|task: &HarborTaskConfig| task.path == trial.task.path)
            {
                tasks.push(trial.task.clone());
            }
            if !agents.iter().any(|agent: &HarborAgentConfig| {
                agent.name == trial.agent.name && agent.model_name == trial.agent.model_name
            }) {
                agents.push(trial.agent.clone());
            }
        }
        Self::write_json(
            &self.root.join("config.json"),
            &HarborJobConfig {
                job_name: self.job_id.to_string(),
                jobs_dir: self.jobs_dir.clone(),
                n_concurrent_trials: self.max_concurrency,
                quiet: true,
                environment: HarborEnvironmentConfig::from(self.environment),
                verifier: HarborVerifierConfig::native(),
                agents,
                tasks,
            },
        )?;
        Self::write_json(
            &self.root.join("lock.json"),
            &HarborJobLock {
                schema_version: 2,
                created_at: self.started_at,
                harbor: HarborLockInfo {},
                n_concurrent_trials: self.max_concurrency,
                retry: HarborRetryConfig::default(),
                trials: recorded.iter().map(|trial| trial.lock.clone()).collect(),
            },
        )
    }

    fn write_json(path: &Path, value: &impl Serialize) -> Result<(), HarborError> {
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        Self::atomic_write(path, bytes)
    }

    fn write_terminal_json(
        path: &Path,
        value: &impl Serialize,
        prerequisites: &[&Path],
        job_root: &Path,
    ) -> Result<(), HarborError> {
        Self::write_terminal_json_with_sync(path, value, prerequisites, job_root, sync_directory)
    }

    fn write_terminal_json_with_sync<F>(
        path: &Path,
        value: &impl Serialize,
        prerequisites: &[&Path],
        job_root: &Path,
        mut sync_directory: F,
    ) -> Result<(), HarborError>
    where
        F: FnMut(&Path) -> std::io::Result<()>,
    {
        for prerequisite in prerequisites {
            Self::sync_terminal_prerequisite_with(prerequisite, &mut sync_directory)?;
        }
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        Self::atomic_write_with_sync(path, bytes, &mut sync_directory)?;
        sync_directory(job_root)?;
        if let Some(output_root) = job_root.parent() {
            sync_directory(output_root)?;
        }
        Ok(())
    }

    fn sync_terminal_prerequisite_with<F>(
        path: &Path,
        sync_directory: &mut F,
    ) -> Result<(), HarborError>
    where
        F: FnMut(&Path) -> std::io::Result<()>,
    {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(HarborError::MissingTerminalPrerequisite(path.to_path_buf()));
            }
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(HarborError::MissingTerminalPrerequisite(path.to_path_buf()));
        }
        file.sync_all()?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    }

    fn read_json_if_exists<T>(path: &Path) -> Result<Option<T>, HarborError>
    where
        T: for<'de> Deserialize<'de>,
    {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), HarborError> {
        let mut sync = sync_directory;
        Self::atomic_write_with_sync(path, bytes, &mut sync)
    }

    fn atomic_write_with_sync<F>(
        path: &Path,
        bytes: impl AsRef<[u8]>,
        sync_directory: &mut F,
    ) -> Result<(), HarborError>
    where
        F: FnMut(&Path) -> std::io::Result<()>,
    {
        require_durable_directory_sync()?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes.as_ref())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        sync_directory(parent)?;
        Ok(())
    }

    fn write_file(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), HarborError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "durable Harbor artifact commits require directory fsync support: {}",
            path.display()
        ),
    ))
}

#[cfg(unix)]
const fn require_durable_directory_sync() -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn require_durable_directory_sync() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "durable Harbor artifact commits require directory fsync support",
    ))
}

fn harbor_agent_config(model: &str, effort: &str) -> HarborAgentConfig {
    HarborAgentConfig {
        name: "nanocodex".to_owned(),
        model_name: format!("openai/{model}"),
        kwargs: HarborAgentKwargs {
            effort: effort.to_owned(),
        },
    }
}

fn harbor_float_key(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

#[derive(Serialize)]
struct HarborInput<'a> {
    protocol_version: u32,
    request_id: Option<String>,
    #[serde(rename = "type")]
    kind: &'static str,
    payload: HarborInputPayload<'a>,
}

#[derive(Serialize)]
struct HarborInputPayload<'a> {
    instruction: &'a str,
}

struct HarborRecordedTrial {
    task: HarborTaskConfig,
    agent: HarborAgentConfig,
    lock: HarborTrialLock,
}

#[derive(Serialize)]
struct HarborJobConfig {
    job_name: String,
    jobs_dir: PathBuf,
    n_concurrent_trials: usize,
    quiet: bool,
    environment: HarborEnvironmentConfig,
    verifier: HarborVerifierConfig,
    agents: Vec<HarborAgentConfig>,
    tasks: Vec<HarborTaskConfig>,
}

#[derive(Deserialize, Serialize)]
struct HarborJobLock {
    schema_version: u32,
    created_at: DateTime<Utc>,
    harbor: HarborLockInfo,
    n_concurrent_trials: usize,
    retry: HarborRetryConfig,
    trials: Vec<HarborTrialLock>,
}

#[derive(Deserialize, Serialize)]
struct HarborLockInfo {}

#[derive(Deserialize, Serialize)]
struct HarborRetryConfig {
    max_retries: u32,
    exclude_exceptions: Vec<String>,
    wait_multiplier: f64,
    min_wait_sec: f64,
    max_wait_sec: f64,
}

impl Default for HarborRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 0,
            exclude_exceptions: [
                "AgentTimeoutError",
                "VerifierTimeoutError",
                "RewardFileNotFoundError",
                "RewardFileEmptyError",
                "VerifierOutputParseError",
                "ApiUsageLimitError",
                "AgentSafetyRefusalError",
                "AgentAuthenticationError",
                "ModelNotFoundError",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            wait_multiplier: 1.0,
            min_wait_sec: 1.0,
            max_wait_sec: 60.0,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborTrialLock {
    schema_version: u32,
    task: HarborTaskLock,
    nanocodex: NanocodexTrialLock,
    install_only: bool,
    timeout_multiplier: f64,
    agent: HarborAgentConfig,
    skills: Vec<HarborAgentSkillLock>,
    environment: HarborEnvironmentConfig,
    verifier: HarborVerifierConfig,
}

impl HarborTrialLock {
    fn new(
        task: &Task,
        model: &str,
        effort: &str,
        task_content_hash: &str,
        materialization_digest: &str,
        environment: EvalEnvironment,
    ) -> Self {
        Self {
            schema_version: 1,
            task: HarborTaskLock {
                name: task
                    .name()
                    .rsplit('/')
                    .next()
                    .unwrap_or(task.name())
                    .to_owned(),
                kind: HarborTaskLockKind::Local,
                digest: format!("sha256:{task_content_hash}"),
                source: Some("nanocodex/local".to_owned()),
                path: task.root().to_path_buf(),
            },
            nanocodex: NanocodexTrialLock {
                materialization_digest_schema: PACKAGE_DIGEST_SCHEMA.to_owned(),
                materialization_digest: format!("sha256:{materialization_digest}"),
                image_reference: task.image().reference().to_owned(),
                verifier_script: task
                    .verifier()
                    .script()
                    .strip_prefix(task.root())
                    .unwrap_or_else(|_| task.verifier().script())
                    .to_path_buf(),
                verifier_environment_mode: task.verifier().environment_mode().as_str().to_owned(),
                verifier_timeout_ns: u64::try_from(task.verifier().timeout().as_nanos())
                    .unwrap_or(u64::MAX),
                scoring_policy: "all_rewards_positive-v1".to_owned(),
            },
            install_only: false,
            timeout_multiplier: 1.0,
            agent: HarborAgentConfig {
                name: "nanocodex".to_owned(),
                model_name: format!("openai/{model}"),
                kwargs: HarborAgentKwargs {
                    effort: effort.to_owned(),
                },
            },
            skills: Vec::new(),
            environment: HarborEnvironmentConfig::from(environment),
            verifier: HarborVerifierConfig::native(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct NanocodexTrialLock {
    materialization_digest_schema: String,
    materialization_digest: String,
    image_reference: String,
    verifier_script: PathBuf,
    verifier_environment_mode: String,
    verifier_timeout_ns: u64,
    scoring_policy: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborTaskLock {
    name: String,
    #[serde(rename = "type")]
    kind: HarborTaskLockKind,
    digest: String,
    source: Option<String>,
    path: PathBuf,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum HarborTaskLockKind {
    Local,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborAgentSkillLock {}

#[derive(Serialize)]
struct HarborTrialConfig<'a> {
    task: HarborTaskConfig,
    trial_name: &'a str,
    trials_dir: &'a Path,
    agent: HarborAgentConfig,
    environment: HarborEnvironmentConfig,
    verifier: HarborVerifierConfig,
    artifacts: Vec<String>,
    extra_instruction_paths: Vec<PathBuf>,
    job_id: Uuid,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborTaskConfig {
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborAgentConfig {
    name: String,
    model_name: String,
    kwargs: HarborAgentKwargs,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborAgentKwargs {
    effort: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborEnvironmentConfig {
    #[serde(rename = "type")]
    environment_type: Option<HarborEnvironmentType>,
    import_path: String,
    delete: bool,
    cpu_enforcement_policy: ResourceMode,
    memory_enforcement_policy: ResourceMode,
    kwargs: NativeEnvironmentKwargs,
}

impl From<EvalEnvironment> for HarborEnvironmentConfig {
    fn from(environment: EvalEnvironment) -> Self {
        let (import_path, backend) = match environment {
            EvalEnvironment::Native => ("nanocodex_eval.native:NativeEnvironment", "native"),
            EvalEnvironment::MicroVm => ("nanocodex_vm:VmEnvironment", "microvm"),
        };
        Self {
            environment_type: None,
            import_path: import_path.to_owned(),
            delete: false,
            cpu_enforcement_policy: ResourceMode::Ignore,
            memory_enforcement_policy: ResourceMode::Ignore,
            kwargs: NativeEnvironmentKwargs {
                backend: backend.to_owned(),
            },
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum HarborEnvironmentType {}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum ResourceMode {
    Ignore,
}

#[derive(Clone, Deserialize, Serialize)]
struct NativeEnvironmentKwargs {
    backend: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborVerifierConfig {
    import_path: String,
}

impl HarborVerifierConfig {
    fn native() -> Self {
        Self {
            import_path: "nanocodex_eval.native:Verifier".to_owned(),
        }
    }
}

#[derive(Serialize)]
struct HarborTrialResult<'a> {
    id: Uuid,
    task_name: &'a str,
    trial_name: &'a str,
    trial_uri: String,
    task_id: HarborTaskId,
    source: &'static str,
    task_checksum: String,
    outcome: EvalOutcome,
    scored: bool,
    cleanup: &'a EvalCleanup,
    config: HarborTrialConfig<'a>,
    agent_info: HarborAgentInfo<'a>,
    agent_result: Option<HarborAgentResult<'a>>,
    verifier_result: Option<HarborVerifierResult<'a>>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    queue_wait: Option<&'a PhaseTiming>,
    environment_setup: Option<&'a PhaseTiming>,
    environment_readiness: Option<&'a PhaseTiming>,
    agent_setup: Option<&'a PhaseTiming>,
    agent_execution: Option<&'a PhaseTiming>,
    verifier: Option<&'a PhaseTiming>,
    exception_info: Option<HarborExceptionInfo<'a>>,
    step_results: Option<Vec<HarborStepResult>>,
}

#[derive(Serialize)]
struct HarborExceptionInfo<'a> {
    exception_type: &'a str,
    exception_message: &'a str,
    exception_traceback: &'a str,
    occurred_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct HarborStepResult {}

#[derive(Serialize)]
struct HarborTaskId {
    path: PathBuf,
}

#[derive(Serialize)]
struct HarborAgentInfo<'a> {
    name: &'static str,
    version: &'static str,
    model_info: HarborModelInfo<'a>,
}

#[derive(Serialize)]
struct HarborModelInfo<'a> {
    name: &'a str,
    provider: &'static str,
}

#[derive(Serialize)]
struct HarborAgentResult<'a> {
    n_input_tokens: u64,
    n_cache_tokens: u64,
    n_output_tokens: u64,
    cost_usd: Option<f64>,
    billing_completeness: BillingCompleteness,
    rollout_details: Option<Vec<HarborRolloutDetail>>,
    metadata: &'a AgentMetadata,
}

#[derive(Serialize)]
struct HarborRolloutDetail {}

#[derive(Serialize)]
struct HarborVerifierResult<'a> {
    exit_code: i32,
    rewards: &'a BTreeMap<String, f64>,
}

#[derive(Serialize)]
struct HarborArtifactManifestEntry {}

#[derive(Clone, Deserialize, Serialize)]
struct HarborJobResult {
    id: Uuid,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    n_total_trials: usize,
    stats: HarborJobStats,
}

struct DurableHarborTrial {
    directory: PathBuf,
    result: RetainedHarborTrialResult,
    coordinate: Option<RunCoordinate>,
    lock: HarborTrialLock,
}

struct DurableResultPath {
    directory: PathBuf,
    result_path: PathBuf,
    coordinate: Option<RunCoordinate>,
}

#[derive(Deserialize)]
struct RetainedHarborTrialResult {
    id: Uuid,
    task_name: String,
    trial_name: String,
    source: String,
    task_checksum: String,
    outcome: EvalOutcome,
    scored: bool,
    cleanup: EvalCleanup,
    config: RetainedHarborTrialConfig,
    agent_info: RetainedHarborAgentInfo,
    agent_result: Option<RetainedHarborAgentResult>,
    verifier_result: Option<RetainedHarborVerifierResult>,
    queue_wait: Option<RetainedPhaseTiming>,
    environment_setup: Option<RetainedPhaseTiming>,
    environment_readiness: Option<RetainedPhaseTiming>,
    agent_setup: Option<RetainedPhaseTiming>,
    agent_execution: Option<RetainedPhaseTiming>,
    verifier: Option<RetainedPhaseTiming>,
    exception_info: Option<RetainedHarborExceptionInfo>,
}

#[derive(Deserialize)]
struct RetainedHarborTrialConfig {
    environment: RetainedHarborEnvironment,
}

#[derive(Deserialize)]
struct RetainedHarborEnvironment {
    kwargs: RetainedHarborEnvironmentKwargs,
}

#[derive(Deserialize)]
struct RetainedHarborEnvironmentKwargs {
    backend: String,
}

#[derive(Deserialize)]
struct RetainedHarborAgentInfo {
    name: String,
    version: String,
    model_info: RetainedHarborModelInfo,
}

#[derive(Deserialize)]
struct RetainedHarborModelInfo {
    name: String,
}

#[derive(Deserialize)]
struct RetainedHarborAgentResult {
    n_input_tokens: u64,
    n_cache_tokens: u64,
    n_output_tokens: u64,
    cost_usd: Option<f64>,
    billing_completeness: BillingCompleteness,
    metadata: AgentMetadata,
}

#[derive(Deserialize)]
struct RetainedHarborVerifierResult {
    exit_code: i32,
    rewards: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct RetainedHarborExceptionInfo {
    exception_type: String,
}

#[derive(Deserialize)]
struct RetainedPhaseTiming {
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
}

impl DurableHarborTrial {
    fn eval_key(&self) -> String {
        format!(
            "{}__{}__{}",
            self.result.agent_info.name, self.result.agent_info.model_info.name, self.result.source
        )
    }

    fn attempt_fact(self, configuration: String, repetition: u16) -> AttemptFact {
        let agent = self.result.agent_result.as_ref();
        let metadata = agent.map(|agent| &agent.metadata);
        let scored = self.result.scored;
        let verifier_passed = self
            .result
            .verifier_result
            .as_ref()
            .is_some_and(|verifier| verifier.rewards.values().all(|reward| *reward > 0.0));
        let outcome = self.result.outcome;
        let passed = scored && verifier_passed;
        let errored = retained_trial_errored(&self.result);
        let refused = retained_trial_refused(&self.result);
        let exception_kind = self
            .result
            .exception_info
            .as_ref()
            .and_then(|exception| retained_exception_kind(&exception.exception_type));
        let billing_snapshot_missing = retained_billing_snapshot_missing(&self.result);
        let cleanup_failed = self.result.cleanup.is_failed()
            || self
                .result
                .exception_info
                .as_ref()
                .is_some_and(|exception| exception.exception_type == "CleanupError");
        let queue_wait_ns = retained_phase_duration_ns(self.result.queue_wait.as_ref());
        let environment_setup_ns =
            retained_phase_duration_ns(self.result.environment_setup.as_ref());
        let environment_readiness_ns =
            retained_phase_duration_ns(self.result.environment_readiness.as_ref());
        let vm_bootstrap_ns = if self.result.config.environment.kwargs.backend == "microvm" {
            environment_readiness_ns
        } else {
            0
        };
        let agent_setup_ns = retained_phase_duration_ns(self.result.agent_setup.as_ref());
        let agent_execution_ns = retained_phase_duration_ns(self.result.agent_execution.as_ref());
        let verifier_ns = retained_phase_duration_ns(self.result.verifier.as_ref());
        let cleanup_ns = [&self.result.cleanup.agent, &self.result.cleanup.verifier]
            .into_iter()
            .filter_map(|cleanup| cleanup.timing.as_ref())
            .map(phase_duration_ns)
            .fold(0_u64, u64::saturating_add);
        let total_ns = [
            queue_wait_ns,
            environment_setup_ns,
            environment_readiness_ns,
            agent_setup_ns,
            agent_execution_ns,
            verifier_ns,
            cleanup_ns,
        ]
        .into_iter()
        .fold(0_u64, u64::saturating_add);
        let nanocodex_lock = &self.lock.nanocodex;
        let model = metadata.map_or_else(
            || {
                self.lock
                    .agent
                    .model_name
                    .strip_prefix("openai/")
                    .unwrap_or(&self.result.agent_info.model_info.name)
                    .to_owned()
            },
            |metadata| metadata.model.clone(),
        );
        let effort = metadata.map_or_else(
            || self.lock.agent.kwargs.effort.clone(),
            |metadata| metadata.effort.clone(),
        );
        let environment = if self.result.config.environment.kwargs.backend == "microvm" {
            EvalEnvironment::MicroVm
        } else {
            EvalEnvironment::Native
        };
        let task_execution =
            metadata.map_or_else(UsageTotals::default, |metadata| metadata.usage.clone());
        let warmup = metadata.map_or_else(UsageTotals::default, |metadata| {
            metadata.warmup_usage.clone()
        });
        let combined = combine_retained_usage(&task_execution, &warmup);
        let usage = retained_usage_observed(agent).then(|| AttemptUsage {
            completeness: if agent
                .is_some_and(|agent| agent.billing_completeness == BillingCompleteness::Complete)
            {
                MeasurementCompleteness::Complete
            } else {
                MeasurementCompleteness::ObservedLowerBound
            },
            task_execution,
            warmup,
            combined,
        });
        let runtime = metadata.map(retained_runtime_metrics);
        let task = AttemptTaskIdentity {
            dataset: self
                .result
                .task_name
                .split_once('/')
                .map(|(dataset, _)| dataset.to_owned()),
            dataset_revision: None,
            name: self.result.task_name.clone(),
            root: self.lock.task.path.clone(),
            package_digest_schema: nanocodex_lock.materialization_digest_schema.clone(),
            package_digest: nanocodex_lock.materialization_digest.clone(),
            harbor_checksum: Some(self.result.task_checksum.clone()),
            image_reference: Some(nanocodex_lock.image_reference.clone()),
            verifier: AttemptVerifierIdentity {
                script: Some(nanocodex_lock.verifier_script.clone()),
                environment_mode: Some(nanocodex_lock.verifier_environment_mode.clone()),
                timeout_ns: Some(nanocodex_lock.verifier_timeout_ns),
                scoring_policy: nanocodex_lock.scoring_policy.clone(),
            },
        };
        let configuration = AttemptConfigurationIdentity {
            id: configuration,
            model,
            model_tier: None,
            reasoning_effort: effort,
            reasoning_mode: metadata.and_then(|metadata| metadata.reasoning_mode.clone()),
            service_tier: metadata
                .and_then(|metadata| metadata.estimated_cost.as_ref())
                .map(|cost| cost.service_tier().as_str().to_owned()),
            transport: metadata.map(|metadata| metadata.transport.clone()),
            orchestration: metadata.map(|metadata| metadata.orchestration.clone()),
            tool_profile: None,
            seed: None,
            agent_topology: "single_agent".to_owned(),
            environment,
            vm: None,
        };
        let verifier = self.result.verifier_result.as_ref().map_or_else(
            AttemptVerifierFact::default,
            |verifier| AttemptVerifierFact {
                exit_code: Some(verifier.exit_code),
                rewards: verifier.rewards.clone(),
            },
        );
        let build = Some(AttemptBuildIdentity {
            version: self.result.agent_info.version.clone(),
            git_sha: None,
            built_at: None,
            executable_sha256: None,
        });
        AttemptFact {
            attempt_id: self.result.id,
            task,
            configuration,
            build,
            repetition,
            outcome,
            scored,
            passed,
            errored,
            refused,
            exception_kind,
            cleanup_failed,
            verifier,
            usage,
            runtime,
            cost_usd: agent.and_then(|agent| agent.cost_usd),
            estimated_cost: metadata.and_then(|metadata| metadata.estimated_cost.clone()),
            billing_completeness: agent.map(|agent| agent.billing_completeness),
            billing_snapshot_missing,
            latency: LatencyBreakdown {
                queue_wait_ns,
                environment_setup_ns,
                environment_readiness_ns,
                vm_bootstrap_ns,
                agent_setup_ns,
                agent_execution_ns,
                model_ns: metadata.map(|metadata| metadata.model_duration_ns),
                tool_work_ns: metadata.map(|metadata| metadata.tool_work_duration_ns),
                tool_wall_ns: metadata.map(|metadata| metadata.tool_wall_duration_ns),
                verifier_ns,
                cleanup_ns,
                total_ns,
                ..LatencyBreakdown::default()
            },
            artifacts: AttemptFactArtifacts {
                result: self.directory.join("result.json"),
                input: self.directory.join("agent/input.jsonl"),
                events: self.directory.join("agent/events.jsonl"),
                trajectory: self.directory.join("agent/trajectory.json"),
                verifier_output: self.directory.join("verifier/test-stdout.txt"),
                workspace: self.directory.join("workspace"),
                lock: self.directory.join("lock.json"),
                directory: self.directory,
            },
        }
    }
}

fn retained_exception_kind(exception_type: &str) -> Option<EvalExceptionKind> {
    match exception_type {
        "AgentSafetyRefusalError" => Some(EvalExceptionKind::AgentSafetyRefusal),
        "AgentAuthenticationError" => Some(EvalExceptionKind::AgentAuthentication),
        "AgentTimeoutError" => Some(EvalExceptionKind::AgentTimeout),
        "VerifierTimeoutError" => Some(EvalExceptionKind::VerifierTimeout),
        "AgentError" => Some(EvalExceptionKind::Agent),
        "VerifierError" => Some(EvalExceptionKind::Verifier),
        "CleanupError" => Some(EvalExceptionKind::Cleanup),
        "EnvironmentError" => Some(EvalExceptionKind::Environment),
        "NanocodexEvalError" => Some(EvalExceptionKind::Internal),
        _ => None,
    }
}

const fn combine_retained_usage(left: &UsageTotals, right: &UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        cached_input_tokens: left
            .cached_input_tokens
            .saturating_add(right.cached_input_tokens),
        cache_write_input_tokens: left
            .cache_write_input_tokens
            .saturating_add(right.cache_write_input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        reasoning_output_tokens: left
            .reasoning_output_tokens
            .saturating_add(right.reasoning_output_tokens),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
    }
}

fn retained_usage_observed(agent: Option<&RetainedHarborAgentResult>) -> bool {
    let Some(agent) = agent else {
        return false;
    };
    let metadata = &agent.metadata;
    agent.cost_usd.is_some()
        || metadata.estimated_cost.is_some()
        || matches!(
            metadata.cost_status.as_str(),
            "estimated_from_usage" | "estimated_lower_bound"
        )
        || agent.n_input_tokens != 0
        || agent.n_cache_tokens != 0
        || agent.n_output_tokens != 0
        || retained_usage_nonzero(&metadata.usage)
        || retained_usage_nonzero(&metadata.warmup_usage)
}

fn retained_billing_snapshot_missing(result: &RetainedHarborTrialResult) -> bool {
    !retained_usage_observed(result.agent_result.as_ref())
        && (result.scored || result.agent_execution.is_some())
}

const fn retained_usage_nonzero(usage: &UsageTotals) -> bool {
    usage.input_tokens != 0
        || usage.cached_input_tokens != 0
        || usage.cache_write_input_tokens != 0
        || usage.output_tokens != 0
        || usage.reasoning_output_tokens != 0
        || usage.total_tokens != 0
}

const fn retained_runtime_metrics(metadata: &AgentMetadata) -> AttemptRuntimeMetrics {
    AttemptRuntimeMetrics {
        completeness: metadata.runtime_completeness,
        model_calls: metadata.model_calls,
        steers: metadata.steers,
        compactions: metadata.compactions,
        tool_calls: metadata.tool_calls,
        connection_attempts: metadata.connection_attempts,
        websocket_reconnects: metadata.websocket_reconnects,
        response_attempts: metadata.response_attempts,
        response_retries: metadata.response_retries,
        billing_uncertain_response_attempts: metadata.billing_uncertain_response_attempts,
        connection_duration_ns: metadata.connection_duration_ns,
        retry_backoff_duration_ns: metadata.retry_backoff_duration_ns,
        model_duration_ns: metadata.model_duration_ns,
        warmup_duration_ns: metadata.warmup_duration_ns,
        tool_work_duration_ns: metadata.tool_work_duration_ns,
        tool_wall_duration_ns: metadata.tool_wall_duration_ns,
    }
}

fn retained_phase_duration_ns(timing: Option<&RetainedPhaseTiming>) -> u64 {
    timing.map_or(0, |timing| {
        retained_duration_ns(timing.started_at, timing.finished_at)
    })
}

fn phase_duration_ns(timing: &PhaseTiming) -> u64 {
    retained_duration_ns(timing.started_at, timing.finished_at)
}

fn retained_duration_ns(started_at: DateTime<Utc>, finished_at: DateTime<Utc>) -> u64 {
    u64::try_from(
        finished_at
            .signed_duration_since(started_at)
            .num_nanoseconds()
            .unwrap_or_default()
            .max(0),
    )
    .unwrap_or(u64::MAX)
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct HarborJobStats {
    n_completed_trials: usize,
    n_errored_trials: usize,
    n_cleanup_failed_trials: usize,
    n_billing_unknown_trials: usize,
    n_billing_missing_trials: usize,
    n_running_trials: usize,
    n_pending_trials: usize,
    n_cancelled_trials: usize,
    n_retries: usize,
    evals: BTreeMap<String, HarborAgentDatasetStats>,
    n_input_tokens: u64,
    n_cache_tokens: u64,
    n_output_tokens: u64,
    cost_usd: Option<f64>,
}

impl HarborJobStats {
    fn from_trials(trials: &[DurableHarborTrial]) -> Self {
        let mut stats = Self {
            n_completed_trials: trials.len(),
            ..Self::default()
        };
        for trial in trials {
            let billing_snapshot_missing = retained_billing_snapshot_missing(&trial.result);
            if let Some(agent) = &trial.result.agent_result {
                stats.n_input_tokens = stats.n_input_tokens.saturating_add(agent.n_input_tokens);
                stats.n_cache_tokens = stats.n_cache_tokens.saturating_add(agent.n_cache_tokens);
                stats.n_output_tokens = stats.n_output_tokens.saturating_add(agent.n_output_tokens);
                if agent.billing_completeness != BillingCompleteness::Complete {
                    stats.n_billing_unknown_trials =
                        stats.n_billing_unknown_trials.saturating_add(1);
                } else if let Some(cost) = agent.cost_usd {
                    stats.cost_usd = Some(stats.cost_usd.unwrap_or_default() + cost);
                }
            }
            if billing_snapshot_missing {
                stats.n_billing_missing_trials = stats.n_billing_missing_trials.saturating_add(1);
            }

            let eval = stats.evals.entry(trial.eval_key()).or_default();
            let scored = trial.result.scored;
            let errored = retained_trial_errored(&trial.result);
            let cleanup_failed = retained_cleanup_failed(&trial.result);
            if cleanup_failed {
                stats.n_cleanup_failed_trials = stats.n_cleanup_failed_trials.saturating_add(1);
                eval.n_cleanup_failures = eval.n_cleanup_failures.saturating_add(1);
            }
            if errored {
                stats.n_errored_trials = stats.n_errored_trials.saturating_add(1);
                eval.n_errors = eval.n_errors.saturating_add(1);
                if let Some(exception) = &trial.result.exception_info {
                    eval.exception_stats
                        .entry(exception.exception_type.clone())
                        .or_default()
                        .push(trial.result.trial_name.clone());
                }
            }
            if scored {
                eval.n_trials = eval.n_trials.saturating_add(1);
                if let Some(verifier) = &trial.result.verifier_result {
                    for (name, reward) in &verifier.rewards {
                        eval.reward_stats
                            .entry(name.clone())
                            .or_default()
                            .entry(harbor_float_key(*reward))
                            .or_default()
                            .push(trial.result.trial_name.clone());
                    }
                }
            }
        }
        stats
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct HarborAgentDatasetStats {
    n_trials: usize,
    n_errors: usize,
    n_cleanup_failures: usize,
    metrics: Vec<HarborMetric>,
    pass_at_k: BTreeMap<usize, f64>,
    reward_stats: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    exception_stats: BTreeMap<String, Vec<String>>,
}

fn compute_harbor_pass_at_k(
    trials: &[DurableHarborTrial],
) -> BTreeMap<String, BTreeMap<usize, f64>> {
    let mut groups = BTreeMap::<String, Option<BTreeMap<String, Vec<u8>>>>::new();
    for trial in trials {
        let success = harbor_binary_success(&trial.result);
        let group = groups
            .entry(trial.eval_key())
            .or_insert_with(|| Some(BTreeMap::new()));
        match (group.as_mut(), success) {
            (Some(tasks), Some(success)) => {
                tasks
                    .entry(trial.result.task_name.clone())
                    .or_default()
                    .push(success);
            }
            (_, None) => *group = None,
            (None, Some(_)) => {}
        }
    }

    groups
        .into_iter()
        .filter_map(|(eval_key, tasks)| {
            compute_pass_at_k_for_tasks(&tasks?).map(|pass_at_k| (eval_key, pass_at_k))
        })
        .collect()
}

fn harbor_binary_success(result: &RetainedHarborTrialResult) -> Option<u8> {
    if !result.scored {
        return Some(0);
    }
    match result.verifier_result.as_ref() {
        None => Some(0),
        Some(verifier) if verifier.rewards.len() == 1 => {
            match verifier
                .rewards
                .values()
                .next()
                .map(|reward| reward.to_bits())
            {
                Some(bits) if bits == 0.0_f64.to_bits() => Some(0),
                Some(bits) if bits == 1.0_f64.to_bits() => Some(1),
                Some(_) | None => None,
            }
        }
        Some(_) => None,
    }
}

fn retained_trial_errored(result: &RetainedHarborTrialResult) -> bool {
    retained_lifecycle_classification(
        result.outcome,
        result
            .exception_info
            .as_ref()
            .map(|exception| exception.exception_type.as_str()),
    )
    .0
}

fn retained_trial_refused(result: &RetainedHarborTrialResult) -> bool {
    retained_lifecycle_classification(
        result.outcome,
        result
            .exception_info
            .as_ref()
            .map(|exception| exception.exception_type.as_str()),
    )
    .1
}

fn retained_lifecycle_classification(
    outcome: EvalOutcome,
    exception_type: Option<&str>,
) -> (bool, bool) {
    match exception_type {
        Some(exception) => (
            exception != "CleanupError",
            exception == "AgentSafetyRefusalError",
        ),
        None => (
            matches!(
                outcome,
                EvalOutcome::SafetyRefusal
                    | EvalOutcome::AgentTimeout
                    | EvalOutcome::InfrastructureError
            ),
            outcome == EvalOutcome::SafetyRefusal,
        ),
    }
}

fn retained_cleanup_failed(result: &RetainedHarborTrialResult) -> bool {
    result.cleanup.is_failed()
        || result
            .exception_info
            .as_ref()
            .is_some_and(|exception| exception.exception_type == "CleanupError")
}

fn compute_pass_at_k_for_tasks(tasks: &BTreeMap<String, Vec<u8>>) -> Option<BTreeMap<usize, f64>> {
    let min_trials = tasks.values().map(Vec::len).min()?;
    let task_count = u32::try_from(tasks.len()).ok()?;
    let mut pass_at_k = BTreeMap::new();
    for k in eligible_k_values(min_trials) {
        let k_u32 = u32::try_from(k).ok()?;
        let mut estimate = 0.0;
        for successes in tasks.values() {
            let n = u32::try_from(successes.len()).ok()?;
            let correct = successes.iter().map(|success| u32::from(*success)).sum();
            estimate += pass_at_k_for_task(n, correct, k_u32);
        }
        pass_at_k.insert(k, estimate / f64::from(task_count));
    }
    Some(pass_at_k)
}

fn eligible_k_values(max_k: usize) -> Vec<usize> {
    let mut values = std::collections::BTreeSet::new();
    let mut k = 2;
    while k <= max_k {
        values.insert(k);
        k *= 2;
    }
    let mut k = 5;
    while k <= max_k {
        values.insert(k);
        k += 5;
    }
    values.into_iter().collect()
}

fn pass_at_k_for_task(n: u32, correct: u32, k: u32) -> f64 {
    if n - correct < k {
        return 1.0;
    }
    let failure_probability = (0..k).fold(1.0, |product, i| {
        product * f64::from(n - correct - i) / f64::from(n - i)
    });
    1.0 - failure_probability
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborMetric {}

#[cfg(all(test, unix))]
#[path = "tests.rs"]
mod tests;

#[cfg(all(test, not(unix)))]
#[path = "unsupported_platform_tests.rs"]
mod unsupported_platform_tests;
