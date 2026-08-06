//! Closed evaluation profiles over native Terminal-Bench task packages.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use nanocodex_oai_api::{Model, Thinking};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
use crate::differential::{CodexToolMode, NanocodexToolMode};
use crate::{Task, TaskLoadError, workset::WorksetSpec};

/// Repository-level native evaluation configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationManifest {
    default: Option<String>,
    #[serde(default)]
    harness: BTreeMap<String, Harness>,
    profiles: BTreeMap<String, Profile>,
}

/// One pinned external evaluation harness.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    command: PathBuf,
}

/// One closed desired bundle of native task coordinates.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    tasks: Vec<PathBuf>,
    #[serde(default)]
    suites: Vec<PathBuf>,
    trials: u16,
    #[serde(default = "default_models")]
    model: Vec<Model>,
    #[serde(
        default = "default_thinking",
        deserialize_with = "deserialize_thinking",
        serialize_with = "serialize_thinking"
    )]
    thinking: Vec<Thinking>,
    #[serde(default)]
    web_search: bool,
    #[serde(default)]
    mode: EvaluationMode,
    #[cfg(any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    ))]
    #[serde(default = "default_nanocodex_tool_modes")]
    nanocodex_tool_mode: Vec<NanocodexToolMode>,
    #[cfg(any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    ))]
    #[serde(default = "default_codex_tool_modes")]
    codex_tool_mode: Vec<CodexToolMode>,
}

/// Execution semantics of one profile treatment.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvaluationMode {
    /// Run only the native Nanocodex evaluator and Harbor verifier.
    #[default]
    Nanocodex,
    /// Run one matched native-Nanocodex versus stock-Codex pair.
    Differential,
}

/// Parsed and content-pinned profile revision.
#[derive(Clone, Debug)]
pub struct ResolvedProfile {
    /// Selected profile name.
    pub name: String,
    /// Stable digest of all resolved profile inputs.
    pub digest: String,
    /// Canonical source manifest path.
    pub config_path: PathBuf,
    /// Loaded immutable task packages.
    pub tasks: Vec<ResolvedTask>,
    /// Exact task/treatment families, excluding fungible repetitions.
    pub families: Vec<ResolvedFamily>,
    /// Optional pinned stock-Codex command required by differential families.
    pub codex_command: Option<PathBuf>,
    /// Whether model-facing web search is enabled.
    pub web_search: bool,
    /// Number of desired repetitions for every family.
    pub trials: u16,
}

/// One profile-visible selector bound to a loaded task package.
#[derive(Clone, Debug)]
pub struct ResolvedTask {
    /// Exact selector accepted by `nanocodex eval run --task`.
    pub selector: String,
    /// Loaded immutable task package.
    pub task: Task,
}

/// One exact semantic treatment family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolvedFamily {
    /// Stable identity excluding fungible repetition.
    pub key: String,
    /// Task selector owned by this family.
    pub task: String,
    /// Execution semantics.
    pub mode: EvaluationMode,
    /// Supported model selection.
    pub model: Model,
    /// Reasoning effort.
    #[serde(serialize_with = "serialize_one_thinking")]
    pub thinking: Thinking,
    /// Nanocodex tool exposure in a matched differential.
    #[cfg(any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    ))]
    pub nanocodex_tool_mode: NanocodexToolMode,
    /// Stock-Codex tool exposure in a matched differential.
    #[cfg(any(
        all(target_os = "linux", not(target_env = "musl")),
        all(target_os = "macos", target_arch = "aarch64")
    ))]
    pub codex_tool_mode: CodexToolMode,
}

/// Profile parsing or resolution failure.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// Manifest could not be read.
    #[error("failed to read evaluation manifest {path}: {source}")]
    Read {
        /// Requested manifest path.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Manifest TOML was invalid.
    #[error("failed to parse evaluation manifest {path}: {source}")]
    Parse {
        /// Requested manifest path.
        path: PathBuf,
        /// TOML decoding failure.
        source: toml::de::Error,
    },
    /// Neither an explicit profile nor a manifest default was available.
    #[error("evaluation profile is required because the manifest has no default")]
    MissingProfile,
    /// The requested profile was absent.
    #[error("evaluation profile `{0}` does not exist")]
    UnknownProfile(String),
    /// Profile had no task inputs.
    #[error("evaluation profile `{0}` contains no tasks or suites")]
    EmptyProfile(String),
    /// Profile requested no repetitions.
    #[error("evaluation profile `{0}` must request at least one trial")]
    ZeroTrials(String),
    /// Profile treatment matrix had an empty dimension.
    #[error("evaluation profile `{profile}` has no {dimension} values")]
    EmptyDimension {
        /// Invalid profile.
        profile: String,
        /// Empty semantic dimension.
        dimension: &'static str,
    },
    /// A suite had no immediate task children.
    #[error("suite contains no immediate task directories: {0}")]
    EmptySuite(PathBuf),
    /// Two task inputs resolved to the same selector.
    #[error("profile contains duplicate task selector `{0}`")]
    DuplicateTask(String),
    /// Two task inputs resolved to the same canonical package.
    #[error("profile contains the task package more than once: {0}")]
    DuplicateTaskRoot(PathBuf),
    /// A task package failed to load.
    #[error(transparent)]
    Task(#[from] TaskLoadError),
    /// Differential execution requires a pinned stock-Codex harness.
    #[error("differential profile `{0}` requires [harness.codex].command")]
    MissingCodex(String),
    /// A resolved path could not be canonicalized.
    #[error("failed to resolve {path}: {source}")]
    ResolvePath {
        /// Path being resolved.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// A pinned harness executable could not be fingerprinted.
    #[error("failed to fingerprint evaluation harness {path}: {source}")]
    FingerprintHarness {
        /// Harness executable being fingerprinted.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Stable identity serialization failed.
    #[error("failed to serialize resolved profile identity: {0}")]
    Identity(#[from] serde_json::Error),
}

/// Closed-profile selector failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProfileSelectionError {
    /// Requested task is outside the profile.
    #[error("task `{selector}` is not part of profile `{profile}`")]
    Task {
        /// Selected profile.
        profile: String,
        /// Rejected selector.
        selector: String,
    },
    /// No treatment matched explicit selectors.
    #[error("no treatment in profile `{profile}` matches task `{task}` and the requested knobs")]
    Treatment {
        /// Selected profile.
        profile: String,
        /// Selected task.
        task: String,
    },
    /// Omitted semantic knobs did not identify one family.
    #[error(
        "task `{task}` has multiple treatments in profile `{profile}`; select model, thinking, \
         and/or tool mode"
    )]
    Ambiguous {
        /// Selected profile.
        profile: String,
        /// Selected task.
        task: String,
    },
}

#[derive(Serialize)]
struct ProfileIdentity<'a> {
    schema: u32,
    name: &'a str,
    profile: &'a Profile,
    tasks: Vec<TaskIdentity<'a>>,
    codex_digest: Option<&'a str>,
}

#[derive(Serialize)]
struct TaskIdentity<'a> {
    selector: &'a str,
    digest: &'a str,
}

impl EvaluationManifest {
    /// Loads a manifest and resolves one immutable profile revision.
    pub fn load_profile(
        path: impl AsRef<Path>,
        requested: Option<&str>,
    ) -> Result<ResolvedProfile, ProfileError> {
        let requested_path = path.as_ref();
        let text = fs::read_to_string(requested_path).map_err(|source| ProfileError::Read {
            path: requested_path.to_path_buf(),
            source,
        })?;
        let manifest: Self = toml::from_str(&text).map_err(|source| ProfileError::Parse {
            path: requested_path.to_path_buf(),
            source,
        })?;
        let config_path =
            requested_path
                .canonicalize()
                .map_err(|source| ProfileError::ResolvePath {
                    path: requested_path.to_path_buf(),
                    source,
                })?;
        manifest.resolve(config_path, requested)
    }

    fn resolve(
        self,
        config_path: PathBuf,
        requested: Option<&str>,
    ) -> Result<ResolvedProfile, ProfileError> {
        let name = requested
            .map(ToOwned::to_owned)
            .or_else(|| self.default.clone())
            .ok_or(ProfileError::MissingProfile)?;
        let profile = self
            .profiles
            .get(&name)
            .ok_or_else(|| ProfileError::UnknownProfile(name.clone()))?;
        validate_profile(&name, profile)?;
        let root = config_path
            .parent()
            .expect("a canonical manifest path has a parent");
        let tasks = load_tasks(root, profile)?;
        let codex_command = if profile.mode == EvaluationMode::Differential {
            Some(resolve_path(
                root,
                &self
                    .harness
                    .get("codex")
                    .ok_or_else(|| ProfileError::MissingCodex(name.clone()))?
                    .command,
            )?)
        } else {
            None
        };
        let codex_digest = codex_command.as_deref().map(harness_digest).transpose()?;
        let families = expand_families(profile, &tasks);
        let identity = ProfileIdentity {
            schema: 2,
            name: &name,
            profile,
            tasks: tasks
                .iter()
                .map(|task| TaskIdentity {
                    selector: &task.selector,
                    digest: task.task.package_digest(),
                })
                .collect(),
            codex_digest: codex_digest.as_deref(),
        };
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&identity)?));
        Ok(ResolvedProfile {
            name,
            digest,
            config_path,
            tasks,
            families,
            codex_command,
            web_search: profile.web_search,
            trials: profile.trials,
        })
    }
}

impl ResolvedProfile {
    /// Converts this immutable revision into the complete SQLite workset
    /// definition before any execution begins.
    pub fn workset_spec(&self) -> WorksetSpec {
        WorksetSpec {
            profile: self.name.clone(),
            digest: self.digest.clone(),
            config_path: self.config_path.clone(),
            tasks: self
                .tasks
                .iter()
                .map(|resolved| crate::workset::WorksetTask {
                    selector: resolved.selector.clone(),
                    name: resolved.task.name().to_owned(),
                    root: resolved.task.root().to_path_buf(),
                    digest: resolved.task.package_digest().to_owned(),
                })
                .collect(),
            families: self
                .families
                .iter()
                .map(|family| crate::workset::WorksetFamily {
                    key: family.key.clone(),
                    task_selector: family.task.clone(),
                    treatment: family.treatment(),
                    trials: self.trials,
                })
                .collect(),
        }
    }

    /// Resolves one exact task selector without permitting ad-hoc expansion.
    pub fn task(&self, selector: &str) -> Result<&ResolvedTask, ProfileSelectionError> {
        self.tasks
            .iter()
            .find(|task| task.selector == selector)
            .ok_or_else(|| ProfileSelectionError::Task {
                profile: self.name.clone(),
                selector: selector.to_owned(),
            })
    }

    /// Resolves an exact task family. Omitted dimensions are accepted only
    /// when the profile contains one unambiguous matching treatment.
    pub fn family(
        &self,
        task: &str,
        model: Option<Model>,
        thinking: Option<Thinking>,
        #[cfg(any(
            all(target_os = "linux", not(target_env = "musl")),
            all(target_os = "macos", target_arch = "aarch64")
        ))]
        nanocodex_tool_mode: Option<NanocodexToolMode>,
        #[cfg(any(
            all(target_os = "linux", not(target_env = "musl")),
            all(target_os = "macos", target_arch = "aarch64")
        ))]
        codex_tool_mode: Option<CodexToolMode>,
    ) -> Result<&ResolvedFamily, ProfileSelectionError> {
        self.task(task)?;
        let matching = self
            .families
            .iter()
            .filter(|family| {
                family.task == task
                    && model.is_none_or(|model| family.model == model)
                    && thinking.is_none_or(|thinking| family.thinking == thinking)
                    && nanocodex_tool_mode.is_none_or(|mode| family.nanocodex_tool_mode == mode)
                    && codex_tool_mode.is_none_or(|mode| family.codex_tool_mode == mode)
            })
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [family] => Ok(family),
            [] => Err(ProfileSelectionError::Treatment {
                profile: self.name.clone(),
                task: task.to_owned(),
            }),
            _ => Err(ProfileSelectionError::Ambiguous {
                profile: self.name.clone(),
                task: task.to_owned(),
            }),
        }
    }
}

impl ResolvedFamily {
    /// Stable serialized treatment retained beside every family.
    pub fn treatment(&self) -> String {
        serde_json::to_string(self).expect("resolved profile families are JSON serializable")
    }
}

fn validate_profile(name: &str, profile: &Profile) -> Result<(), ProfileError> {
    if profile.trials == 0 {
        return Err(ProfileError::ZeroTrials(name.to_owned()));
    }
    if profile.tasks.is_empty() && profile.suites.is_empty() {
        return Err(ProfileError::EmptyProfile(name.to_owned()));
    }
    for (values, dimension) in [
        (profile.model.len(), "model"),
        (profile.thinking.len(), "thinking"),
    ] {
        if values == 0 {
            return Err(ProfileError::EmptyDimension {
                profile: name.to_owned(),
                dimension,
            });
        }
    }
    Ok(())
}

fn load_tasks(root: &Path, profile: &Profile) -> Result<Vec<ResolvedTask>, ProfileError> {
    let mut inputs = profile
        .tasks
        .iter()
        .map(|path| {
            (
                path.to_string_lossy().into_owned(),
                resolve_path(root, path),
            )
        })
        .map(|(selector, path)| path.map(|path| (selector, path)))
        .collect::<Result<Vec<_>, _>>()?;
    for suite in &profile.suites {
        let suite_root = resolve_path(root, suite)?;
        let entries = fs::read_dir(&suite_root).map_err(|source| ProfileError::Read {
            path: suite_root.clone(),
            source,
        })?;
        let mut children = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ProfileError::Read {
                path: suite_root.clone(),
                source,
            })?;
            let kind = entry.file_type().map_err(|source| ProfileError::Read {
                path: entry.path(),
                source,
            })?;
            let path = entry.path();
            if kind.is_dir() && path.join("task.toml").is_file() {
                children.push(path);
            }
        }
        children.sort();
        if children.is_empty() {
            return Err(ProfileError::EmptySuite(suite_root));
        }
        let prefix = suite.to_string_lossy();
        inputs.extend(children.into_iter().map(|path| {
            let name = path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            (format!("{prefix}/{name}"), path)
        }));
    }
    let mut selectors = BTreeSet::new();
    let mut roots = BTreeSet::new();
    inputs
        .into_iter()
        .map(|(selector, path)| {
            if !selectors.insert(selector.clone()) {
                return Err(ProfileError::DuplicateTask(selector));
            }
            if !roots.insert(path.clone()) {
                return Err(ProfileError::DuplicateTaskRoot(path));
            }
            Ok(ResolvedTask {
                selector,
                task: Task::load(path)?,
            })
        })
        .collect()
}

#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
fn expand_families(profile: &Profile, tasks: &[ResolvedTask]) -> Vec<ResolvedFamily> {
    let mut families = Vec::new();
    for task in tasks {
        for model in &profile.model {
            for thinking in &profile.thinking {
                for nanocodex_tool_mode in &profile.nanocodex_tool_mode {
                    for codex_tool_mode in &profile.codex_tool_mode {
                        let key = format!(
                            "{}|{}|{}|{}|{}|{}",
                            task.selector,
                            profile.mode.as_str(),
                            model.as_str(),
                            thinking.as_str(),
                            nanocodex_tool_mode.as_str(),
                            codex_tool_mode.as_str()
                        );
                        families.push(ResolvedFamily {
                            key,
                            task: task.selector.clone(),
                            mode: profile.mode,
                            model: *model,
                            thinking: *thinking,
                            nanocodex_tool_mode: *nanocodex_tool_mode,
                            codex_tool_mode: *codex_tool_mode,
                        });
                    }
                }
            }
        }
    }
    families
}

#[cfg(not(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
)))]
fn expand_families(_profile: &Profile, _tasks: &[ResolvedTask]) -> Vec<ResolvedFamily> {
    Vec::new()
}

impl EvaluationMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Nanocodex => "nanocodex",
            Self::Differential => "differential",
        }
    }
}

fn resolve_path(root: &Path, path: &Path) -> Result<PathBuf, ProfileError> {
    let requested = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    requested
        .canonicalize()
        .map_err(|source| ProfileError::ResolvePath {
            path: requested,
            source,
        })
}

fn harness_digest(path: &Path) -> Result<String, ProfileError> {
    let mut file = fs::File::open(path).map_err(|source| ProfileError::FingerprintHarness {
        path: path.to_path_buf(),
        source,
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| ProfileError::FingerprintHarness {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn default_models() -> Vec<Model> {
    vec![Model::default()]
}

fn default_thinking() -> Vec<Thinking> {
    vec![Thinking::default()]
}

fn deserialize_thinking<'de, D>(deserializer: D) -> Result<Vec<Thinking>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| value.parse().map_err(serde::de::Error::custom))
        .collect()
}

fn serialize_thinking<S>(values: &[Thinking], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    values
        .iter()
        .map(|value| value.as_str())
        .collect::<Vec<_>>()
        .serialize(serializer)
}

pub(crate) fn serialize_one_thinking<S>(value: &Thinking, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(value.as_str())
}

#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
fn default_nanocodex_tool_modes() -> Vec<NanocodexToolMode> {
    vec![NanocodexToolMode::default()]
}

#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    all(target_os = "macos", target_arch = "aarch64")
))]
fn default_codex_tool_modes() -> Vec<CodexToolMode> {
    vec![CodexToolMode::CodeModeOnly]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(root: &Path, name: &str) {
        let task = root.join(name);
        fs::create_dir_all(task.join("environment")).unwrap();
        fs::create_dir_all(task.join("tests")).unwrap();
        fs::write(
            task.join("task.toml"),
            format!(
                r#"schema_version = "1.1"
[task]
name = "{name}"
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
"#
            ),
        )
        .unwrap();
        fs::write(task.join("instruction.md"), "do it").unwrap();
        fs::write(task.join("environment/Dockerfile"), "FROM scratch").unwrap();
        fs::write(task.join("tests/test.sh"), "#!/bin/sh\n").unwrap();
    }

    #[test]
    fn profile_expands_trials_in_sqlite_but_not_as_agent_selectors() {
        let directory = tempfile::tempdir().unwrap();
        write_task(directory.path(), "one");
        let config = directory.path().join("nanocodex.toml");
        fs::write(
            &config,
            r#"default = "release"
[profiles.release]
tasks = ["one"]
trials = 3
model = ["sol"]
thinking = ["high"]
"#,
        )
        .unwrap();

        let profile = EvaluationManifest::load_profile(&config, None).unwrap();
        assert_eq!(profile.name, "release");
        assert_eq!(profile.families.len(), 1);
        assert_eq!(profile.workset_spec().families[0].trials, 3);
        assert_eq!(profile.task("one").unwrap().task.name(), "one");
    }

    #[test]
    fn task_selector_cannot_expand_the_profile() {
        let directory = tempfile::tempdir().unwrap();
        write_task(directory.path(), "included");
        write_task(directory.path(), "outside");
        let config = directory.path().join("nanocodex.toml");
        fs::write(
            &config,
            r#"[profiles.release]
tasks = ["included"]
trials = 1
"#,
        )
        .unwrap();
        let profile = EvaluationManifest::load_profile(&config, Some("release")).unwrap();

        assert!(matches!(
            profile.task("outside"),
            Err(ProfileSelectionError::Task { selector, .. }) if selector == "outside"
        ));
    }

    #[test]
    fn differential_profile_revision_pins_the_harness_bytes() {
        let directory = tempfile::tempdir().unwrap();
        write_task(directory.path(), "one");
        let codex = directory.path().join("codex");
        fs::write(&codex, "first build").unwrap();
        let config = directory.path().join("nanocodex.toml");
        fs::write(
            &config,
            r#"[harness.codex]
command = "codex"

[profiles.release]
tasks = ["one"]
trials = 1
mode = "differential"
"#,
        )
        .unwrap();

        let first = EvaluationManifest::load_profile(&config, Some("release")).unwrap();
        fs::write(&codex, "second build").unwrap();
        let second = EvaluationManifest::load_profile(&config, Some("release")).unwrap();

        assert_ne!(first.digest, second.digest);
    }

    #[test]
    fn profile_revision_is_independent_of_the_checkout_path() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for directory in [&first, &second] {
            write_task(directory.path(), "one");
            fs::write(
                directory.path().join("nanocodex.toml"),
                r#"[profiles.release]
tasks = ["one"]
trials = 2
model = ["sol"]
thinking = ["high"]
"#,
            )
            .unwrap();
        }

        let first =
            EvaluationManifest::load_profile(first.path().join("nanocodex.toml"), Some("release"))
                .unwrap();
        let second =
            EvaluationManifest::load_profile(second.path().join("nanocodex.toml"), Some("release"))
                .unwrap();

        assert_eq!(first.digest, second.digest);
    }
}
