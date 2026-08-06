//! Profile-level durable execution API.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::{Path, PathBuf},
    time::Duration,
};

use nanocodex_oai_api::{Model, Thinking};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::task::JoinHandle;

use crate::{
    Task,
    profile::{EvaluationManifest, ResolvedFamily, ResolvedHarness, ResolvedProfile},
    workset::{BeginCoordinate, CoordinateLease, PreparationLease, Workset, WorksetBusy},
};

const LEDGER_FILE: &str = "state.sqlite3";

/// One initialized profile revision and its durable SQLite ledger.
#[derive(Clone, Debug)]
pub struct Evaluation {
    profile: ResolvedProfile,
    workset: Workset,
    state_directory: PathBuf,
}

/// Optional knobs selecting one exact family already present in a profile.
#[derive(Clone, Debug)]
pub struct EvaluationSelector {
    task: String,
    harness: Option<String>,
    model: Option<Model>,
    thinking: Option<Thinking>,
}

/// One exact profile family resolved locally without opening a ledger.
#[derive(Clone, Debug)]
pub struct EvaluationSelection {
    profile: String,
    profile_digest: String,
    family_key: String,
    task: Task,
    treatment: EvaluationTreatment,
    web_search: bool,
    harness: Option<ResolvedHarness>,
    harnesses: Vec<ResolvedHarness>,
}

/// The next durable action for one profile family.
#[derive(Debug)]
pub enum EvaluationClaim {
    /// Prepare immutable resources shared by every trial of this task.
    Prepare(PreparationClaim),
    /// Execute one SQLite-allocated trial.
    Run(CoordinateClaim),
    /// Matching work exists but another process currently owns it.
    Busy(EvaluationBusy),
    /// Every trial in the selected family has an accepted result.
    Complete,
}

/// Leased ownership of shared task preparation.
#[derive(Debug)]
pub struct PreparationClaim {
    workset: Workset,
    lease: PreparationLease,
    task: Task,
    harnesses: Vec<ResolvedHarness>,
    heartbeat: JoinHandle<()>,
}

/// Leased ownership of one fungible profile trial.
#[derive(Debug)]
pub struct CoordinateClaim {
    workset: Workset,
    lease: CoordinateLease,
    task: Task,
    treatment: EvaluationTreatment,
    web_search: bool,
    harness: Option<ResolvedHarness>,
    harnesses: Vec<ResolvedHarness>,
    output_directory: PathBuf,
    heartbeat: JoinHandle<()>,
}

/// Semantic knobs fixed by one profile family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EvaluationTreatment {
    /// Built-in or configured harness used for this coordinate.
    pub harness: String,
    /// Model fixed by the profile.
    pub model: Model,
    /// Reasoning effort fixed by the profile.
    #[serde(serialize_with = "crate::profile::serialize_one_thinking")]
    pub thinking: Thinking,
}

/// Temporary inability to claim the selected family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EvaluationBusy {
    /// Stable machine-readable reason.
    pub reason: &'static str,
    /// Suggested delay before retrying.
    pub retry_after_ms: u64,
}

/// Complete durable status of one immutable profile revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EvaluationStatus {
    /// Selected profile name.
    pub profile: String,
    /// Digest of the profile, tasks, harness, and treatments.
    pub digest: String,
    /// Shared task-preparation counts.
    pub preparation: EvaluationCounts,
    /// Trial execution counts.
    pub coordinates: EvaluationCounts,
    /// Status grouped by exact semantic treatment.
    pub families: Vec<EvaluationFamilyStatus>,
}

/// Counts for one durable work state machine.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct EvaluationCounts {
    /// Work available to claim or reclaim.
    pub pending: i64,
    /// Work with a live lease.
    pub running: i64,
    /// Work with an accepted terminal result.
    pub complete: i64,
}

/// Durable status of one exact profile treatment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EvaluationFamilyStatus {
    /// Stable family identity.
    pub id: String,
    /// Profile-visible task selector.
    pub task: String,
    /// Host currently assigned to prepare and execute this task.
    pub assigned_host: Option<String>,
    /// Semantic treatment fixed by the profile.
    pub treatment: EvaluationTreatment,
    /// Desired fungible trial count.
    pub desired: i64,
    /// Trials available to claim or reclaim.
    pub pending: i64,
    /// Trials with a live lease.
    pub running: i64,
    /// Trials with accepted results.
    pub complete: i64,
}

/// Profile resolution, selection, or durable-ledger failure.
#[derive(Debug)]
pub struct EvaluationError {
    source: Box<dyn Error + Send + Sync>,
}

impl Evaluation {
    /// Resolves a profile, initializes its complete workset, and opens it.
    pub fn open(
        config: impl AsRef<Path>,
        profile: Option<&str>,
        state_directory: impl Into<PathBuf>,
    ) -> Result<Self, EvaluationError> {
        let profile = EvaluationManifest::load_profile(config, profile).map_err(error)?;
        let state_directory = state_directory.into();
        let workset = Workset::ensure(state_directory.join(LEDGER_FILE), &profile.workset_spec())
            .map_err(error)?;
        Ok(Self {
            profile,
            workset,
            state_directory,
        })
    }

    /// Selected profile name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.profile.name
    }

    /// Whether model-facing web search is enabled by the profile.
    #[must_use]
    pub const fn web_search(&self) -> bool {
        self.profile.web_search
    }

    /// Reads a structured snapshot from SQLite.
    pub fn status(&self) -> Result<EvaluationStatus, EvaluationError> {
        let status = self.workset.status().map_err(error)?;
        let families = status
            .families
            .into_iter()
            .map(|status| -> Result<_, EvaluationError> {
                let family = self
                    .profile
                    .families
                    .iter()
                    .find(|family| family.key == status.key)
                    .ok_or_else(|| {
                        error(std::io::Error::other(format!(
                            "SQLite contains unknown profile family `{}`",
                            status.key
                        )))
                    })?;
                Ok(EvaluationFamilyStatus {
                    id: status.key,
                    task: status.task,
                    assigned_host: status.assigned_host,
                    treatment: family.into(),
                    desired: status.desired,
                    pending: status.pending,
                    running: status.running,
                    complete: status.terminal,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EvaluationStatus {
            profile: status.profile,
            digest: status.digest,
            preparation: EvaluationCounts {
                pending: status.preparation.pending,
                running: status.preparation.running,
                complete: status.preparation.complete,
            },
            coordinates: EvaluationCounts {
                pending: status.coordinates.pending,
                running: status.coordinates.running,
                complete: status.coordinates.terminal,
            },
            families,
        })
    }

    /// Claims the next action for one exact profile-owned family.
    ///
    /// Active claims renew their lease automatically until completed, retried,
    /// or dropped.
    pub fn claim(
        &self,
        selector: &EvaluationSelector,
        lease_duration: Duration,
    ) -> Result<EvaluationClaim, EvaluationError> {
        self.claim_for_host(selector, "local", lease_duration)
    }

    /// Claims one family for the network host chosen by the coordinator.
    pub(crate) fn claim_for_host(
        &self,
        selector: &EvaluationSelector,
        host: &str,
        lease_duration: Duration,
    ) -> Result<EvaluationClaim, EvaluationError> {
        let family = self.resolve_family(selector)?.clone();
        self.claim_resolved(&family, host, lease_duration)
    }

    /// Claims a locally resolved family after verifying its immutable profile digest.
    pub(crate) fn claim_family_for_host(
        &self,
        profile_digest: &str,
        family_key: &str,
        host: &str,
        lease_duration: Duration,
    ) -> Result<EvaluationClaim, EvaluationError> {
        if profile_digest != self.profile.digest {
            return Err(error(std::io::Error::other(format!(
                "coordinator profile digest is {}; worker requested {profile_digest}",
                self.profile.digest
            ))));
        }
        let family = self
            .profile
            .families
            .iter()
            .find(|family| family.key == family_key)
            .ok_or_else(|| {
                error(std::io::Error::other(format!(
                    "family `{family_key}` is not part of profile `{}`",
                    self.profile.name
                )))
            })?
            .clone();
        self.claim_resolved(&family, host, lease_duration)
    }

    fn resolve_family(
        &self,
        selector: &EvaluationSelector,
    ) -> Result<&ResolvedFamily, EvaluationError> {
        self.profile
            .family(
                &selector.task,
                selector.harness.as_deref(),
                selector.model,
                selector.thinking,
            )
            .map_err(error)
    }

    fn claim_resolved(
        &self,
        family: &ResolvedFamily,
        host: &str,
        lease_duration: Duration,
    ) -> Result<EvaluationClaim, EvaluationError> {
        let task = self.profile.task(&family.task).map_err(error)?.task.clone();
        match self
            .workset
            .begin_for_host(&family.key, host, lease_duration)
            .map_err(error)?
        {
            BeginCoordinate::Prepare(lease) => {
                let heartbeat =
                    preparation_heartbeat(self.workset.clone(), lease.clone(), lease_duration);
                Ok(EvaluationClaim::Prepare(PreparationClaim {
                    workset: self.workset.clone(),
                    lease,
                    task,
                    harnesses: self.profile.harnesses.values().cloned().collect(),
                    heartbeat,
                }))
            }
            BeginCoordinate::Execute(lease) => {
                let output_directory = coordinate_output(
                    &self.state_directory,
                    &self.profile.digest,
                    &family.key,
                    lease.repetition,
                    lease.generation,
                );
                let heartbeat =
                    coordinate_heartbeat(self.workset.clone(), lease.clone(), lease_duration);
                Ok(EvaluationClaim::Run(CoordinateClaim {
                    workset: self.workset.clone(),
                    lease,
                    task,
                    treatment: family.into(),
                    web_search: self.profile.web_search,
                    harness: self.profile.harness(&family.harness).cloned(),
                    harnesses: self.profile.harnesses.values().cloned().collect(),
                    output_directory,
                    heartbeat,
                }))
            }
            BeginCoordinate::Busy(busy) => Ok(EvaluationClaim::Busy(busy.into())),
            BeginCoordinate::Complete => Ok(EvaluationClaim::Complete),
        }
    }
}

impl EvaluationSelection {
    /// Resolves one exact family from a local immutable profile without touching SQLite.
    pub fn load(
        config: impl AsRef<Path>,
        profile: Option<&str>,
        selector: &EvaluationSelector,
    ) -> Result<Self, EvaluationError> {
        let profile = EvaluationManifest::load_profile(config, profile).map_err(error)?;
        let family = profile
            .family(
                &selector.task,
                selector.harness.as_deref(),
                selector.model,
                selector.thinking,
            )
            .map_err(error)?
            .clone();
        let task = profile.task(&family.task).map_err(error)?.task.clone();
        let harness = profile.harness(&family.harness).cloned();
        let harnesses = profile.harnesses.values().cloned().collect();
        Ok(Self {
            profile: profile.name,
            profile_digest: profile.digest,
            family_key: family.key.clone(),
            task,
            treatment: (&family).into(),
            web_search: profile.web_search,
            harness,
            harnesses,
        })
    }

    /// Selected profile name.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Stable digest of every resolved profile input.
    #[must_use]
    pub fn profile_digest(&self) -> &str {
        &self.profile_digest
    }

    /// Stable family key sent to the coordinator.
    #[must_use]
    pub fn family_key(&self) -> &str {
        &self.family_key
    }

    /// Immutable task package executed by this worker.
    #[must_use]
    pub const fn task(&self) -> &Task {
        &self.task
    }

    /// Exact semantic treatment fixed by the profile.
    #[must_use]
    pub const fn treatment(&self) -> &EvaluationTreatment {
        &self.treatment
    }

    /// Whether model-facing web search is enabled.
    #[must_use]
    pub const fn web_search(&self) -> bool {
        self.web_search
    }

    /// Resolved configuration for the selected external harness.
    #[must_use]
    pub const fn harness(&self) -> Option<&ResolvedHarness> {
        self.harness.as_ref()
    }

    /// External harnesses installed during this task's durable preparation.
    #[must_use]
    pub fn harnesses(&self) -> &[ResolvedHarness] {
        &self.harnesses
    }
}

impl EvaluationSelector {
    /// Selects a task from the closed profile.
    #[must_use]
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            harness: None,
            model: None,
            thinking: None,
        }
    }

    /// Selects one configured external harness. Omission selects Nanocodex.
    #[must_use]
    pub fn harness(mut self, harness: Option<impl Into<String>>) -> Self {
        self.harness = harness.map(Into::into);
        self
    }

    /// Narrows the task to one profile-owned model treatment.
    #[must_use]
    pub const fn model(mut self, model: Option<Model>) -> Self {
        self.model = model;
        self
    }

    /// Narrows the task to one profile-owned reasoning treatment.
    #[must_use]
    pub const fn thinking(mut self, thinking: Option<Thinking>) -> Self {
        self.thinking = thinking;
        self
    }
}

impl PreparationClaim {
    /// Immutable task package requiring shared preparation.
    #[must_use]
    pub const fn task(&self) -> &Task {
        &self.task
    }

    /// External harnesses installed into the immutable task image.
    #[must_use]
    pub fn harnesses(&self) -> &[ResolvedHarness] {
        &self.harnesses
    }

    /// Accepts successful preparation if this claim still owns the lease.
    pub fn complete(self) -> Result<(), EvaluationError> {
        self.heartbeat.abort();
        self.workset
            .complete_preparation(&self.lease)
            .map_err(error)
    }

    /// Releases failed preparation for retry while retaining its diagnostic.
    pub fn retry(self, failure: &str) -> Result<(), EvaluationError> {
        self.heartbeat.abort();
        self.workset
            .retry_preparation(&self.lease, failure)
            .map_err(error)
    }
}

impl CoordinateClaim {
    /// Immutable task package for this trial.
    #[must_use]
    pub const fn task(&self) -> &Task {
        &self.task
    }

    /// Semantic treatment fixed by the profile.
    #[must_use]
    pub const fn treatment(&self) -> &EvaluationTreatment {
        &self.treatment
    }

    /// Internal fungible repetition allocated by SQLite.
    #[must_use]
    pub const fn repetition(&self) -> u16 {
        self.lease.repetition
    }

    /// Whether model-facing web search is enabled by the profile.
    #[must_use]
    pub const fn web_search(&self) -> bool {
        self.web_search
    }

    /// Resolved configuration for the selected external harness.
    #[must_use]
    pub const fn harness(&self) -> Option<&ResolvedHarness> {
        self.harness.as_ref()
    }

    /// External harnesses installed into the immutable task image.
    #[must_use]
    pub fn harnesses(&self) -> &[ResolvedHarness] {
        &self.harnesses
    }

    /// Unique retained-artifact directory for this profile trial.
    #[must_use]
    pub fn output_directory(&self) -> &Path {
        &self.output_directory
    }

    /// Accepts one terminal result if this claim still owns the lease.
    pub fn complete(self, evidence: &Path) -> Result<(), EvaluationError> {
        self.heartbeat.abort();
        self.workset
            .complete_coordinate(&self.lease, evidence)
            .map_err(error)
    }

    /// Releases a failed trial for retry while retaining its diagnostic.
    pub fn retry(self, failure: &str) -> Result<(), EvaluationError> {
        self.heartbeat.abort();
        self.workset
            .retry_coordinate(&self.lease, failure)
            .map_err(error)
    }
}

impl From<&ResolvedFamily> for EvaluationTreatment {
    fn from(family: &ResolvedFamily) -> Self {
        Self {
            harness: family.harness.clone(),
            model: family.model,
            thinking: family.thinking,
        }
    }
}

impl From<WorksetBusy> for EvaluationBusy {
    fn from(busy: WorksetBusy) -> Self {
        Self {
            reason: busy.reason,
            retry_after_ms: busy.retry_after_ms,
        }
    }
}

impl Display for EvaluationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.source, formatter)
    }
}

impl Error for EvaluationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn error(source: impl Error + Send + Sync + 'static) -> EvaluationError {
    EvaluationError {
        source: Box::new(source),
    }
}

fn coordinate_output(
    state_directory: &Path,
    profile_digest: &str,
    family_key: &str,
    repetition: u16,
    generation: i64,
) -> PathBuf {
    let family_digest = hex::encode(Sha256::digest(family_key.as_bytes()));
    state_directory
        .join("artifacts")
        .join(profile_digest)
        .join(family_digest)
        .join(format!("k-{repetition}"))
        .join(format!("attempt-{generation}"))
}

fn heartbeat_interval(lease_duration: Duration) -> Duration {
    Duration::from_secs((lease_duration.as_secs() / 10).clamp(1, 30))
}

fn preparation_heartbeat(
    workset: Workset,
    lease: PreparationLease,
    lease_duration: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval(lease_duration));
        interval.tick().await;
        loop {
            interval.tick().await;
            if workset
                .heartbeat_preparation(&lease, lease_duration)
                .is_err()
            {
                return;
            }
        }
    })
}

fn coordinate_heartbeat(
    workset: Workset,
    lease: CoordinateLease,
    lease_duration: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval(lease_duration));
        interval.tick().await;
        loop {
            interval.tick().await;
            if workset
                .heartbeat_coordinate(&lease, lease_duration)
                .is_err()
            {
                return;
            }
        }
    })
}

impl Drop for PreparationClaim {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}

impl Drop for CoordinateClaim {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    fn write_task(root: &Path) {
        let task = root.join("one");
        fs::create_dir_all(task.join("environment")).unwrap();
        fs::create_dir_all(task.join("tests")).unwrap();
        fs::write(
            task.join("task.toml"),
            r#"schema_version = "1.1"
[task]
name = "one"
description = "test"
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "alpine:3.21"
cpus = 1
memory_mb = 128
storage_mb = 128
gpus = 0
allow_internet = false
"#,
        )
        .unwrap();
        fs::write(task.join("instruction.md"), "do it").unwrap();
        fs::write(task.join("environment/Dockerfile"), "FROM scratch").unwrap();
        fs::write(task.join("tests/test.sh"), "#!/bin/sh\n").unwrap();
    }

    #[tokio::test]
    async fn one_handle_owns_profile_expansion_claims_and_completion() {
        let directory = tempfile::tempdir().unwrap();
        write_task(directory.path());
        let config = directory.path().join("nanocodex.toml");
        fs::write(
            &config,
            r#"[profiles.release]
tasks = ["one"]
trials = 2
model = ["sol"]
thinking = ["high"]
"#,
        )
        .unwrap();
        let state = directory.path().join("state");
        let evaluation = Evaluation::open(&config, Some("release"), &state).unwrap();
        let selector = EvaluationSelector::new("one");

        let status = evaluation.status().unwrap();
        assert_eq!(status.coordinates.pending, 2);
        assert_eq!(status.families[0].desired, 2);
        assert_eq!(status.families[0].treatment.harness, "nanocodex");

        let EvaluationClaim::Prepare(preparation) = evaluation
            .claim(&selector, Duration::from_secs(30))
            .unwrap()
        else {
            panic!("first claim should own preparation");
        };
        assert_eq!(preparation.task().name(), "one");
        preparation.complete().unwrap();

        let EvaluationClaim::Run(coordinate) = evaluation
            .claim(&selector, Duration::from_secs(30))
            .unwrap()
        else {
            panic!("second claim should own a trial");
        };
        assert_eq!(coordinate.repetition(), 1);
        assert!(
            coordinate
                .output_directory()
                .starts_with(state.join("artifacts"))
        );
        coordinate.complete(Path::new("accepted-result")).unwrap();

        let status = evaluation.status().unwrap();
        assert_eq!(status.coordinates.complete, 1);
        assert_eq!(status.coordinates.pending, 1);
    }

    #[tokio::test]
    async fn selectors_cannot_expand_the_profile() {
        let directory = tempfile::tempdir().unwrap();
        write_task(directory.path());
        let config = directory.path().join("nanocodex.toml");
        fs::write(
            &config,
            r#"[profiles.release]
tasks = ["one"]
trials = 1
"#,
        )
        .unwrap();
        let evaluation =
            Evaluation::open(&config, Some("release"), directory.path().join("state")).unwrap();

        let failure = evaluation
            .claim(&EvaluationSelector::new("outside"), Duration::from_secs(30))
            .unwrap_err();
        assert!(failure.to_string().contains("is not part of profile"));
    }
}
