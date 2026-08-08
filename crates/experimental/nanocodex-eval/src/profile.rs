//! Optional workset recipes and runtime harness helpers.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use nanocodex_oai_api::{Model, Thinking};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{Task, TaskLoadError};

pub(crate) const BUILTIN_HARNESS: &str = "nanocodex";

/// Repository-level native evaluation configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationManifest {
    default: Option<String>,
    #[serde(default)]
    harness: BTreeMap<String, Harness>,
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

/// One configured external evaluation harness helper.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    command: PathBuf,
    /// Absolute executable path installed in every prepared task image.
    guest_command: String,
    /// Semantic harness release shared by architecture-specific executables.
    ///
    /// When omitted, legacy manifests continue to pin the exact command bytes.
    version: Option<String>,
    /// Complete guest argument template for the harness JSONL contract.
    arguments: Vec<String>,
    /// Harness-specific guest environment variables.
    #[serde(default)]
    environment: BTreeMap<String, String>,
    /// Guest home exposed to the harness and available as a template value.
    #[serde(default = "default_harness_home")]
    home: String,
    /// Guest destination for file-based credentials.
    #[serde(default = "default_harness_auth_file")]
    auth_file: String,
    /// Guest environment variable receiving API-key credentials.
    #[serde(default = "default_harness_api_key_environment")]
    api_key_environment: String,
    /// OpenAI-compatible upstream reached through the capture proxy.
    api_upstream: Option<String>,
}

/// One convenience recipe expanded into SQLite by `eval add`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    tasks: Vec<PathBuf>,
    #[serde(default)]
    suites: Vec<PathBuf>,
    trials: u16,
    #[serde(default = "default_harnesses")]
    harness: Vec<String>,
    #[serde(default = "default_models")]
    model: Vec<Model>,
    #[serde(
        default = "default_thinking",
        deserialize_with = "deserialize_thinking"
    )]
    thinking: Vec<Thinking>,
    #[serde(default)]
    web_search: bool,
}

/// Parsed profile recipe ready to materialize into SQLite.
#[derive(Clone, Debug)]
pub struct ResolvedProfile {
    /// Loaded immutable task packages.
    pub tasks: Vec<ResolvedTask>,
    /// Exact task/treatment families, excluding fungible repetitions.
    pub families: Vec<ResolvedFamily>,
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

/// One external harness helper resolved from the current runtime config.
#[derive(Clone, Debug)]
pub struct ResolvedHarness {
    /// Profile-visible harness name.
    pub name: String,
    /// Architecture-local executable.
    pub command: PathBuf,
    /// Executable path inside the prepared task image.
    pub guest_command: String,
    /// Guest argument template.
    pub arguments: Vec<String>,
    /// Guest environment additions.
    pub environment: BTreeMap<String, String>,
    /// Generic writable home staged for this harness.
    pub home: String,
    /// Guest destination for file-based credentials.
    pub auth_file: String,
    /// Guest environment variable receiving API-key credentials.
    pub api_key_environment: String,
    /// Capture-proxy upstream override.
    pub api_upstream: Option<String>,
    /// Semantic version retained in evidence.
    pub version: String,
}

/// One exact semantic treatment family.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedFamily {
    /// Stable identity excluding fungible repetition.
    pub key: String,
    /// Task selector owned by this family.
    pub task: String,
    /// Built-in or configured harness used for this coordinate.
    pub harness: String,
    /// Supported model selection.
    pub model: Model,
    /// Reasoning effort.
    #[serde(
        deserialize_with = "deserialize_one_thinking",
        serialize_with = "serialize_one_thinking"
    )]
    pub thinking: Thinking,
    /// Whether model-facing web search is enabled for this treatment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub web_search: bool,
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
    /// A profile repeated one harness name.
    #[error("evaluation profile `{profile}` contains duplicate harness `{harness}`")]
    DuplicateHarness {
        /// Invalid profile.
        profile: String,
        /// Repeated harness name.
        harness: String,
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
    /// A selected external harness has no current runtime helper.
    #[error("evaluation harness helper `{0}` is not configured")]
    UnknownHarness(String),
    /// A semantic harness version was present but empty.
    #[error("evaluation harness `{0}` has an empty version")]
    EmptyHarnessVersion(String),
    /// A harness guest command was not an absolute path.
    #[error("evaluation harness `{harness}` guest_command must be a clean absolute path: {path}")]
    InvalidHarnessGuestCommand {
        /// Invalid harness name.
        harness: String,
        /// Rejected guest path.
        path: String,
    },
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
}

impl EvaluationManifest {
    /// Loads and expands one optional profile recipe.
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

    /// Resolves one named external harness from the current helper config.
    /// Built-in Nanocodex execution requires no helper entry.
    pub fn load_harness(
        path: impl AsRef<Path>,
        name: &str,
    ) -> Result<Option<ResolvedHarness>, ProfileError> {
        if name == BUILTIN_HARNESS {
            return Ok(None);
        }
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
        let harness = manifest
            .harness
            .get(name)
            .ok_or_else(|| ProfileError::UnknownHarness(name.to_owned()))?;
        resolve_harness(
            config_path
                .parent()
                .expect("a canonical config path has a parent"),
            name,
            harness,
        )
        .map(Some)
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
        let families = expand_families(profile, &tasks);
        Ok(ResolvedProfile {
            tasks,
            families,
            trials: profile.trials,
        })
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
        (profile.harness.len(), "harness"),
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
    let mut harnesses = BTreeSet::new();
    for harness in &profile.harness {
        if !harnesses.insert(harness) {
            return Err(ProfileError::DuplicateHarness {
                profile: name.to_owned(),
                harness: harness.clone(),
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

fn expand_families(profile: &Profile, tasks: &[ResolvedTask]) -> Vec<ResolvedFamily> {
    let mut families = Vec::new();
    for task in tasks {
        for harness in &profile.harness {
            for model in &profile.model {
                for thinking in &profile.thinking {
                    let key = format!(
                        "{}|{}|{}|{}{}",
                        task.selector,
                        harness,
                        model.as_str(),
                        thinking.as_str(),
                        if profile.web_search {
                            "|web-search"
                        } else {
                            ""
                        },
                    );
                    families.push(ResolvedFamily {
                        key,
                        task: task.selector.clone(),
                        harness: harness.clone(),
                        model: *model,
                        thinking: *thinking,
                        web_search: profile.web_search,
                    });
                }
            }
        }
    }
    families
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

fn resolve_harness(
    root: &Path,
    name: &str,
    harness: &Harness,
) -> Result<ResolvedHarness, ProfileError> {
    if !Path::new(&harness.guest_command).is_absolute()
        || harness.guest_command.chars().any(char::is_whitespace)
        || !Path::new(&harness.guest_command)
            .components()
            .all(|component| {
                matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
    {
        return Err(ProfileError::InvalidHarnessGuestCommand {
            harness: name.to_owned(),
            path: harness.guest_command.clone(),
        });
    }
    let command = resolve_path(root, &harness.command)?;
    let command_identity = match harness.version.as_deref() {
        Some(version) if version.trim().is_empty() => {
            return Err(ProfileError::EmptyHarnessVersion(name.to_owned()));
        }
        Some(version) => format!("version:{version}"),
        None => harness_digest(&command)?,
    };
    Ok(ResolvedHarness {
        name: name.to_owned(),
        command,
        guest_command: harness.guest_command.clone(),
        arguments: harness.arguments.clone(),
        environment: harness.environment.clone(),
        home: harness.home.clone(),
        auth_file: harness.auth_file.clone(),
        api_key_environment: harness.api_key_environment.clone(),
        api_upstream: harness.api_upstream.clone(),
        version: harness.version.clone().unwrap_or(command_identity),
    })
}

fn default_models() -> Vec<Model> {
    vec![Model::default()]
}

fn default_harnesses() -> Vec<String> {
    vec![BUILTIN_HARNESS.to_owned()]
}

fn default_harness_home() -> String {
    "/run/nanocodex-harness-home".to_owned()
}

fn default_harness_auth_file() -> String {
    "/run/nanocodex-harness-home/auth.json".to_owned()
}

fn default_harness_api_key_environment() -> String {
    "OPENAI_API_KEY".to_owned()
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

pub(crate) fn serialize_one_thinking<S>(value: &Thinking, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(value.as_str())
}

fn deserialize_one_thinking<'de, D>(deserializer: D) -> Result<Thinking, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
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
        assert_eq!(profile.families.len(), 1);
        assert_eq!(profile.trials, 3);
        assert_eq!(profile.tasks[0].task.name(), "one");
    }

    #[test]
    fn profile_recipe_retains_only_the_harness_name() {
        let directory = tempfile::tempdir().unwrap();
        write_task(directory.path(), "one");
        let codex = directory.path().join("codex");
        fs::write(&codex, "first build").unwrap();
        let config = directory.path().join("nanocodex.toml");
        fs::write(
            &config,
            r#"[harness.codex]
command = "codex"
guest_command = "/usr/local/bin/codex"
arguments = ["{prompt}"]

[profiles.release]
tasks = ["one"]
trials = 1
harness = ["nanocodex", "codex"]
"#,
        )
        .unwrap();

        let profile = EvaluationManifest::load_profile(&config, Some("release")).unwrap();
        assert_eq!(profile.families.len(), 2);
        assert_eq!(profile.families[0].harness, "nanocodex");
        assert_eq!(profile.families[1].harness, "codex");
        let first_helper = EvaluationManifest::load_harness(&config, "codex")
            .unwrap()
            .unwrap();
        fs::write(&codex, "second build").unwrap();
        let second = EvaluationManifest::load_profile(&config, Some("release")).unwrap();
        let second_helper = EvaluationManifest::load_harness(&config, "codex")
            .unwrap()
            .unwrap();

        assert_eq!(second.families, profile.families);
        assert_ne!(first_helper.version, second_helper.version);
    }

    #[test]
    fn external_harness_can_pin_one_version_across_architecture_builds() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for (directory, command) in [(&first, "aarch64 build"), (&second, "x86_64 build")] {
            write_task(directory.path(), "one");
            fs::write(directory.path().join("codex"), command).unwrap();
            fs::write(
                directory.path().join("nanocodex.toml"),
                r#"[harness.codex]
command = "codex"
guest_command = "/usr/local/bin/codex"
version = "0.145.0"
arguments = ["{prompt}"]

[profiles.release]
tasks = ["one"]
trials = 1
harness = ["nanocodex", "codex"]
"#,
            )
            .unwrap();
        }

        let first = EvaluationManifest::load_harness(first.path().join("nanocodex.toml"), "codex")
            .unwrap()
            .unwrap();
        let second =
            EvaluationManifest::load_harness(second.path().join("nanocodex.toml"), "codex")
                .unwrap()
                .unwrap();

        assert_eq!(first.version, "0.145.0");
        assert_eq!(first.version, second.version);
    }
}
