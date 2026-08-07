//! SQLite-native durable evaluation API.

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
    profile::{EvaluationManifest, ResolvedFamily, ResolvedHarness, ResolvedTask},
    workset::{
        BeginCoordinate, CoordinateLease, PreparationLease, Workset, WorksetBusy, WorksetError,
        WorksetFamily, WorksetTask,
    },
};

const LEDGER_FILE: &str = "state.sqlite3";

/// One named durable SQLite workset.
#[derive(Clone, Debug)]
pub struct Evaluation {
    name: String,
    generation: String,
    tasks: Vec<ResolvedTask>,
    families: Vec<ResolvedFamily>,
    workset: Workset,
    state_directory: PathBuf,
    config: PathBuf,
}

/// Optional knobs selecting one exact family already present in a workset.
#[derive(Clone, Debug)]
pub struct EvaluationSelector {
    task: String,
    harness: Option<String>,
    model: Option<Model>,
    thinking: Option<Thinking>,
    web_search: Option<bool>,
}

/// One concrete task treatment to append to a durable SQLite workset.
#[derive(Clone, Debug)]
pub struct EvaluationWork {
    selector: String,
    task: Task,
    harness: String,
    model: Model,
    thinking: Thinking,
    web_search: bool,
    trials: u16,
}

/// The next durable action for one workset family.
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
    treatment: EvaluationTreatment,
    harnesses: Vec<ResolvedHarness>,
    heartbeat: JoinHandle<()>,
}

/// Leased ownership of one fungible workset trial.
#[derive(Debug)]
pub struct CoordinateClaim {
    workset: Workset,
    lease: CoordinateLease,
    task: Task,
    treatment: EvaluationTreatment,
    harness: Option<ResolvedHarness>,
    harnesses: Vec<ResolvedHarness>,
    output_directory: PathBuf,
    heartbeat: JoinHandle<()>,
}

/// Semantic knobs fixed by one SQLite family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EvaluationTreatment {
    /// Built-in or configured harness used for this coordinate.
    pub harness: String,
    /// Model fixed by the workset.
    pub model: Model,
    /// Reasoning effort fixed by the workset.
    #[serde(serialize_with = "crate::profile::serialize_one_thinking")]
    pub thinking: Thinking,
    /// Whether model-facing web search is enabled for this coordinate.
    pub web_search: bool,
}

/// Temporary inability to claim the selected family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EvaluationBusy {
    /// Stable machine-readable reason.
    pub reason: &'static str,
    /// Suggested delay before retrying.
    pub retry_after_ms: u64,
}

/// Complete durable status of one named workset generation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EvaluationStatus {
    /// Selected profile name.
    pub profile: String,
    /// Stable identifier of the newest selected generation.
    pub generation: String,
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

/// Durable status of one exact workset treatment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EvaluationFamilyStatus {
    /// Stable family identity.
    pub id: String,
    /// User-visible task selector.
    pub task: String,
    /// Host currently assigned to prepare and execute this task.
    pub assigned_host: Option<String>,
    /// Semantic treatment fixed by the workset.
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

/// Profile expansion, workset selection, or durable-ledger failure.
#[derive(Debug)]
pub struct EvaluationError {
    source: Box<dyn Error + Send + Sync>,
}

impl Evaluation {
    /// Appends concrete work to a named SQLite board, creating it when absent.
    /// By default this extends the latest generation. `new_generation` starts
    /// a fresh generation under the same user-visible profile name.
    pub fn add(
        state_directory: impl Into<PathBuf>,
        workset_name: &str,
        work: &[EvaluationWork],
        new_generation: bool,
    ) -> Result<(), EvaluationError> {
        if work.is_empty() {
            return Err(error(std::io::Error::other(
                "at least one concrete evaluation treatment is required",
            )));
        }
        let path = state_directory.into().join(LEDGER_FILE);
        let workset = if new_generation {
            Workset::create(&path, workset_name)
        } else {
            match Workset::open(&path, workset_name) {
                Ok(workset) => Ok(workset),
                Err(WorksetError::UnknownWorkset(_)) => Workset::create(&path, workset_name),
                Err(error) => Err(error),
            }
        }
        .map_err(error)?;
        let mut tasks = std::collections::BTreeMap::new();
        let mut families = Vec::with_capacity(work.len());
        for item in work {
            if item.trials == 0 {
                return Err(error(std::io::Error::other(
                    "evaluation treatments must request at least one trial",
                )));
            }
            tasks
                .entry(item.selector.clone())
                .or_insert_with(|| WorksetTask {
                    selector: item.selector.clone(),
                    name: item.task.name().to_owned(),
                    root: item.task.root().to_path_buf(),
                    digest: item.task.package_digest().to_owned(),
                });
            let family = item.family();
            families.push(WorksetFamily {
                key: family.key.clone(),
                task_selector: family.task.clone(),
                treatment: family.treatment(),
                trials: item.trials,
            });
        }
        workset
            .append(&tasks.into_values().collect::<Vec<_>>(), &families)
            .map_err(error)
    }

    /// Expands one optional TOML profile recipe into a concrete SQLite board.
    pub fn add_profile(
        config: impl AsRef<Path>,
        recipe: Option<&str>,
        state_directory: impl Into<PathBuf>,
        workset_name: &str,
        new_generation: bool,
    ) -> Result<(), EvaluationError> {
        let recipe = EvaluationManifest::load_profile(config, recipe).map_err(error)?;
        let mut work = Vec::with_capacity(recipe.families.len());
        for task in &recipe.tasks {
            for family in recipe
                .families
                .iter()
                .filter(|family| family.task == task.selector)
            {
                work.push(EvaluationWork {
                    selector: family.task.clone(),
                    task: task.task.clone(),
                    harness: family.harness.clone(),
                    model: family.model,
                    thinking: family.thinking,
                    web_search: family.web_search,
                    trials: recipe.trials,
                });
            }
        }
        Self::add(state_directory, workset_name, &work, new_generation)
    }

    /// Resolves one current runtime harness helper without consulting workset
    /// definitions.
    pub fn resolve_harness(
        config: impl AsRef<Path>,
        harness: &str,
    ) -> Result<Option<ResolvedHarness>, EvaluationError> {
        EvaluationManifest::load_harness(config, harness).map_err(error)
    }

    /// Opens the newest SQLite generation of a named workset.
    ///
    /// `config` is retained only to resolve a selected external harness when a
    /// coordinate is claimed. Status and work definition come entirely from
    /// SQLite.
    pub fn open(
        config: impl AsRef<Path>,
        workset_name: &str,
        state_directory: impl Into<PathBuf>,
    ) -> Result<Self, EvaluationError> {
        let state_directory = state_directory.into();
        let workset =
            Workset::open(state_directory.join(LEDGER_FILE), workset_name).map_err(error)?;
        let definition = workset.definition().map_err(error)?;
        let mut tasks = Vec::with_capacity(definition.tasks.len());
        for retained in definition.tasks {
            let task = Task::load(&retained.root).map_err(error)?;
            if task.name() != retained.name || task.package_digest() != retained.digest {
                return Err(error(std::io::Error::other(format!(
                    "retained task `{}` no longer matches SQLite content digest {}",
                    retained.selector, retained.digest
                ))));
            }
            tasks.push(ResolvedTask {
                selector: retained.selector,
                task,
            });
        }
        let mut families = Vec::with_capacity(definition.families.len());
        for retained in definition.families {
            let family: ResolvedFamily =
                serde_json::from_str(&retained.treatment).map_err(error)?;
            if family.key != retained.key || family.task != retained.task_selector {
                return Err(error(std::io::Error::other(format!(
                    "invalid SQLite treatment for family `{}`",
                    retained.key
                ))));
            }
            families.push(family);
        }
        Ok(Self {
            name: definition.name,
            generation: definition.generation,
            tasks,
            families,
            workset,
            state_directory,
            config: config.as_ref().to_path_buf(),
        })
    }

    /// Selected workset name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reads a structured snapshot from SQLite.
    pub fn status(&self) -> Result<EvaluationStatus, EvaluationError> {
        let status = self.workset.status().map_err(error)?;
        let families = status
            .families
            .into_iter()
            .map(|status| -> Result<_, EvaluationError> {
                let family = self
                    .families
                    .iter()
                    .find(|family| family.key == status.key)
                    .ok_or_else(|| {
                        error(std::io::Error::other(format!(
                            "SQLite contains unknown workset family `{}`",
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
            profile: status.name,
            generation: status.generation,
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

    /// Claims the next action for one exact SQLite-owned family.
    ///
    /// Active claims renew their lease automatically until completed, retried,
    /// or dropped.
    pub fn claim(
        &self,
        selector: &EvaluationSelector,
        lease_duration: Duration,
    ) -> Result<EvaluationClaim, EvaluationError> {
        let family = self.resolve_family(selector)?.clone();
        self.claim_resolved(&family, "local", lease_duration, true)
    }

    /// Claims one family for the network host chosen by the coordinator.
    pub(crate) fn claim_for_host(
        &self,
        selector: &EvaluationSelector,
        host: &str,
        lease_duration: Duration,
    ) -> Result<EvaluationClaim, EvaluationError> {
        let family = self.resolve_family(selector)?.clone();
        self.claim_resolved(&family, host, lease_duration, false)
    }

    fn resolve_family(
        &self,
        selector: &EvaluationSelector,
    ) -> Result<&ResolvedFamily, EvaluationError> {
        self.task(&selector.task)?;
        let harness = selector
            .harness
            .as_deref()
            .unwrap_or(crate::profile::BUILTIN_HARNESS);
        let matching = self
            .families
            .iter()
            .filter(|family| {
                family.task == selector.task
                    && family.harness == harness
                    && selector.model.is_none_or(|model| family.model == model)
                    && selector
                        .thinking
                        .is_none_or(|thinking| family.thinking == thinking)
                    && selector
                        .web_search
                        .is_none_or(|web_search| family.web_search == web_search)
            })
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [family] => Ok(family),
            [] => Err(error(std::io::Error::other(format!(
                "no treatment in profile `{}` matches task `{}` and the requested knobs",
                self.name, selector.task
            )))),
            _ => Err(error(std::io::Error::other(format!(
                "task `{}` has multiple treatments in profile `{}`; select model and/or thinking",
                selector.task, self.name
            )))),
        }
    }

    fn claim_resolved(
        &self,
        family: &ResolvedFamily,
        host: &str,
        lease_duration: Duration,
        resolve_harness: bool,
    ) -> Result<EvaluationClaim, EvaluationError> {
        let task = self.task(&family.task)?.task.clone();
        let harness = if resolve_harness {
            EvaluationManifest::load_harness(&self.config, &family.harness).map_err(error)?
        } else {
            None
        };
        let harnesses = harness.iter().cloned().collect::<Vec<_>>();
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
                    treatment: family.into(),
                    harnesses,
                    heartbeat,
                }))
            }
            BeginCoordinate::Execute(lease) => {
                let output_directory = coordinate_output(
                    &self.state_directory,
                    &self.generation,
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
                    harness,
                    harnesses,
                    output_directory,
                    heartbeat,
                }))
            }
            BeginCoordinate::Busy(busy) => Ok(EvaluationClaim::Busy(busy.into())),
            BeginCoordinate::Complete => Ok(EvaluationClaim::Complete),
        }
    }

    fn task(&self, selector: &str) -> Result<&ResolvedTask, EvaluationError> {
        self.tasks
            .iter()
            .find(|task| task.selector == selector)
            .ok_or_else(|| {
                error(std::io::Error::other(format!(
                    "task `{selector}` is not part of profile `{}`",
                    self.name
                )))
            })
    }
}

impl EvaluationWork {
    /// Creates one built-in Nanocodex treatment with default model policy.
    #[must_use]
    pub fn new(selector: impl Into<String>, task: Task) -> Self {
        Self {
            selector: selector.into(),
            task,
            harness: crate::profile::BUILTIN_HARNESS.to_owned(),
            model: Model::default(),
            thinking: Thinking::default(),
            web_search: false,
            trials: 1,
        }
    }

    /// Selects the built-in or configured harness name stored in SQLite.
    #[must_use]
    pub fn harness(mut self, harness: impl Into<String>) -> Self {
        self.harness = harness.into();
        self
    }

    /// Selects the model stored in SQLite.
    #[must_use]
    pub const fn model(mut self, model: Model) -> Self {
        self.model = model;
        self
    }

    /// Selects the reasoning effort stored in SQLite.
    #[must_use]
    pub const fn thinking(mut self, thinking: Thinking) -> Self {
        self.thinking = thinking;
        self
    }

    /// Selects model-facing web-search policy stored in SQLite.
    #[must_use]
    pub const fn web_search(mut self, web_search: bool) -> Self {
        self.web_search = web_search;
        self
    }

    /// Selects the desired fungible repetition count stored in SQLite.
    #[must_use]
    pub const fn trials(mut self, trials: u16) -> Self {
        self.trials = trials;
        self
    }

    fn family(&self) -> ResolvedFamily {
        let key = format!(
            "{}|{}|{}|{}{}",
            self.selector,
            self.harness,
            self.model.as_str(),
            self.thinking.as_str(),
            if self.web_search { "|web-search" } else { "" },
        );
        ResolvedFamily {
            key,
            task: self.selector.clone(),
            harness: self.harness.clone(),
            model: self.model,
            thinking: self.thinking,
            web_search: self.web_search,
        }
    }
}

impl EvaluationSelector {
    /// Selects a task already present in the workset.
    #[must_use]
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            harness: None,
            model: None,
            thinking: None,
            web_search: None,
        }
    }

    /// Selects one configured external harness. Omission selects Nanocodex.
    #[must_use]
    pub fn harness(mut self, harness: Option<impl Into<String>>) -> Self {
        self.harness = harness.map(Into::into);
        self
    }

    /// Narrows the task to one SQLite-owned model treatment.
    #[must_use]
    pub const fn model(mut self, model: Option<Model>) -> Self {
        self.model = model;
        self
    }

    /// Narrows the task to one SQLite-owned reasoning treatment.
    #[must_use]
    pub const fn thinking(mut self, thinking: Option<Thinking>) -> Self {
        self.thinking = thinking;
        self
    }

    /// Narrows the task to one SQLite-owned web-search policy.
    #[must_use]
    pub const fn web_search(mut self, web_search: Option<bool>) -> Self {
        self.web_search = web_search;
        self
    }

    pub(crate) fn task(&self) -> &str {
        &self.task
    }

    pub(crate) fn harness_name(&self) -> Option<&str> {
        self.harness.as_deref()
    }

    pub(crate) const fn model_value(&self) -> Option<Model> {
        self.model
    }

    pub(crate) const fn thinking_value(&self) -> Option<Thinking> {
        self.thinking
    }

    pub(crate) const fn web_search_value(&self) -> Option<bool> {
        self.web_search
    }
}

impl PreparationClaim {
    /// Immutable task package requiring shared preparation.
    #[must_use]
    pub const fn task(&self) -> &Task {
        &self.task
    }

    /// Semantic treatment whose resources are being prepared.
    #[must_use]
    pub const fn treatment(&self) -> &EvaluationTreatment {
        &self.treatment
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
        self.treatment.web_search
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
            web_search: family.web_search,
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
    workset_generation: &str,
    family_key: &str,
    repetition: u16,
    generation: i64,
) -> PathBuf {
    let family_digest = hex::encode(Sha256::digest(family_key.as_bytes()));
    state_directory
        .join("artifacts")
        .join(workset_generation)
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
        Evaluation::add_profile(&config, Some("release"), &state, "release", false).unwrap();
        let evaluation = Evaluation::open(&config, "release", &state).unwrap();
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
        let state = directory.path().join("state");
        Evaluation::add_profile(&config, Some("release"), &state, "release", false).unwrap();
        let evaluation = Evaluation::open(&config, "release", state).unwrap();

        let failure = evaluation
            .claim(&EvaluationSelector::new("outside"), Duration::from_secs(30))
            .unwrap_err();
        assert!(failure.to_string().contains("is not part of profile"));
    }

    #[test]
    fn sqlite_board_survives_recipe_and_harness_config_removal() {
        let directory = tempfile::tempdir().unwrap();
        write_task(directory.path());
        let config = directory.path().join("nanocodex.toml");
        fs::write(
            &config,
            r#"[profiles.release]
tasks = ["one"]
trials = 3
harness = ["codex"]
model = ["sol"]
thinking = ["high"]
"#,
        )
        .unwrap();
        let state = directory.path().join("state");

        Evaluation::add_profile(&config, Some("release"), &state, "board", false).unwrap();
        fs::remove_file(&config).unwrap();
        let evaluation = Evaluation::open(&config, "board", &state).unwrap();
        let status = evaluation.status().unwrap();

        assert_eq!(status.coordinates.pending, 3);
        assert_eq!(status.families[0].treatment.harness, "codex");
        assert_eq!(
            status.families[0].treatment.model,
            "sol".parse::<Model>().unwrap()
        );
        let failure = evaluation
            .claim(
                &EvaluationSelector::new("one").harness(Some("codex")),
                Duration::from_secs(30),
            )
            .unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("failed to read evaluation manifest")
        );
    }
}
