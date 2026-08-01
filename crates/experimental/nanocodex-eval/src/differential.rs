use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    error::Error,
    fmt::{self, Display, Formatter, Write as _},
    fs::{self, File},
    future::Future,
    io::{self, BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener},
    num::NonZeroUsize,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use futures_util::{StreamExt as _, stream::FuturesUnordered};
use nanocodex_agent::{NanocodexBuilder, Thinking, events::AgentEventKind};
use nanocodex_oai_api::MODEL;
use nanocodex_vm::host::Gvproxy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::{
    io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _},
    sync::mpsc,
    task::JoinHandle,
    time::Instant,
};
use tracing::{info, warn};
use uuid::Uuid;

pub use crate::codex::CodexToolMode;

use crate::{
    AgentResult, AtifBuilder, AtifSource, AtifStep, AtifToolCall, AtifTrajectory,
    CodexCommandOutput, CodexCommandRunner, CodexCommandRunnerError, CodexCommandStatus, CodexExec,
    EvalAttemptOutcome, EvalEventKind, EvalEventStream, EvalExceptionKind, EvalOutcome, EvalStatus,
    Evaluator, EvaluatorBuilder, MeasurementCompleteness, ResponsesCaptureProxy,
    ResponsesCaptureProxyConfig, ResponsesModelCatalogOverride, Task, UsageTotals,
    evaluator::{
        AdmissionAttempt, AdmissionController, AdmissionPermit, AttemptAgent, EvalAttempt,
    },
    project_codex_atif,
    vm::{
        SharedDirectory, VmAttempt, VmAttemptError, VmAttemptMemory, VmAttemptMemorySnapshot,
        VmBackend, VmCommand, VmEnvironment, VmResources, VmToolSessionError, VmToolSessionHandle,
        reflink_or_sparse_copy,
    },
};

type BoxError = Box<dyn Error + Send + Sync + 'static>;
type InternalResult<T, E = BoxError> = std::result::Result<T, E>;

macro_rules! diff_error {
    ($message:literal $(, $argument:expr)* $(,)?) => {
        boxed_message(format!($message $(, $argument)*))
    };
    ($error:expr $(,)?) => {
        boxed_message($error.to_string())
    };
}

fn boxed_message(message: impl Into<String>) -> BoxError {
    Box::new(io::Error::other(message.into()))
}

#[derive(Debug)]
struct ContextError {
    context: String,
    source: BoxError,
}

impl Display for ContextError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.context)
    }
}

impl Error for ContextError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

trait WrapErr<T> {
    fn wrap_err(self, context: impl Into<String>) -> InternalResult<T>;

    fn wrap_err_with(self, context: impl FnOnce() -> String) -> InternalResult<T>;
}

impl<T, E> WrapErr<T> for std::result::Result<T, E>
where
    E: Error + Send + Sync + 'static,
{
    fn wrap_err(self, context: impl Into<String>) -> InternalResult<T> {
        self.map_err(|source| {
            Box::new(ContextError {
                context: context.into(),
                source: Box::new(source),
            }) as BoxError
        })
    }

    fn wrap_err_with(self, context: impl FnOnce() -> String) -> InternalResult<T> {
        self.map_err(|source| {
            Box::new(ContextError {
                context: context(),
                source: Box::new(source),
            }) as BoxError
        })
    }
}

const DEFAULT_OUTPUT_DIRECTORY: &str = ".nanocodex/eval-diff";
const COMPARISON_FILE: &str = "comparison.json";
const COMPARISON_SCHEMA_VERSION: u32 = 16;
const SWEEP_MANIFEST_FILE: &str = "differential-sweep.json";
const SWEEP_LOCK_FILE: &str = ".differential-sweep.lock";
const SWEEP_MANIFEST_SCHEMA_VERSION: u32 = 3;
const PROGRESS_FILE: &str = "progress.jsonl";
const PROGRESS_SCHEMA_VERSION: u32 = 1;
const PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const PROGRESS_HEARTBEAT_SUMMARY_CHARS: usize = 64;
const PROGRESS_SUMMARY_CHARS: usize = 180;
const DIFFERENTIAL_UPPER_DISK_SUFFIX: &str = ".upper.ext4";
const TRAJECTORY_FILE: &str = "agent/trajectory.json";
const API_EXCHANGES_FILE: &str = "agent/api-exchanges.jsonl";
const API_COMPARISON_FILE: &str = "api-comparison.json";
const API_CAPTURE_SCHEMA_VERSION: u32 = 1;
const API_COMPARISON_SCHEMA_VERSION: u32 = 15;
const MODEL_VISIBLE_TOOL_CALL_MEASUREMENT: &str = "responses_output_item_done";
const DIFF_CODEX_SHARE_TAG: &str = "nanoeval-codex";
const DIFF_CODEX_SHARE_MOUNT: &str = "/run/nanoeval-codex";
const DIFF_CODEX_GUEST_BINARY: &str = "/run/nanoeval-codex/codex";
const DIFF_CAPTURE_PROXY_API_UPSTREAM: &str = "https://api.openai.com/v1";
const DIFF_CAPTURE_PROXY_CHATGPT_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";
const DIFF_CAPTURE_PROXY_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const DIFF_API_EXCHANGES_FILENAME: &str = "api-exchanges.jsonl";
const DIFF_CODEX_HOME: &str = "/run/nanoeval-codex-home";
const DIFF_CODEX_AUTH_FILE: &str = "/run/nanoeval-codex-home/auth.json";
const DIFF_CODEX_CLOUD_CONFIG_CACHE_FILENAME: &str = "cloud-config-bundle-cache.json";
const DIFF_CODEX_CLOUD_CONFIG_CACHE_FILE: &str =
    "/run/nanoeval-codex-home/cloud-config-bundle-cache.json";
const DIFF_CODEX_CA_BUNDLE_FILENAME: &str = "ca-certificates.pem";
const DIFF_CODEX_CA_BUNDLE_FILE: &str = "/run/nanoeval-codex/ca-certificates.pem";
const DIFF_CODEX_CA_CERTIFICATE_ENVIRONMENT: &str = "CODEX_CA_CERTIFICATE";
const DIFF_CODEX_SSL_CERT_FILE_ENVIRONMENT: &str = "SSL_CERT_FILE";
const DIFF_CODEX_NIX_SSL_CERT_FILE_ENVIRONMENT: &str = "NIX_SSL_CERT_FILE";
const DIFF_CODEX_LIVE_STDOUT_FILE: &str = "/run/nanoeval-codex-home/codex-live-events.jsonl";
const DIFF_CODEX_LIVE_STDERR_FILE: &str = "/run/nanoeval-codex-home/codex-live-stderr.log";
const DIFF_CODEX_PROGRESS_POLL: Duration = Duration::from_millis(500);
const DIFF_CODEX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const DIFF_CODEX_VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const DIFFERENTIAL_ARMS_PER_PAIR: usize = 2;
const DEFAULT_DIFFERENTIAL_GUEST_MEMORY_MB: u64 = 512;
const MINIMUM_DIFFERENTIAL_GUEST_MEMORY_MB: u64 = 128;
const MEMORY_RECOMMENDATION_PERCENT: u64 = 120;
const MEMORY_RECOMMENDATION_FIXED_SLACK_MB: u64 = 64;
const MEMORY_PROFILE_SCHEMA_VERSION: u32 = 1;
#[cfg(target_arch = "aarch64")]
const VM_GUEST_TARGET: &str = "aarch64-unknown-linux-musl";
#[cfg(target_arch = "x86_64")]
const VM_GUEST_TARGET: &str = "x86_64-unknown-linux-musl";
#[cfg(target_arch = "aarch64")]
const VM_GUEST_ELF_MACHINE: u16 = 183;
#[cfg(target_arch = "x86_64")]
const VM_GUEST_ELF_MACHINE: u16 = 62;

/// Nanocodex's model-visible tool treatment in a differential evaluation.
///
/// This report/configuration value is intentionally owned by the evaluator.
/// It converts to the tools crate's runtime policy only when an arm launches.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NanocodexToolMode {
    /// Expose normal tools directly as well as through Code Mode.
    CodeMode,
    /// Expose normal tools only through Code Mode's `exec` entrypoint.
    #[default]
    CodeModeOnly,
}

impl NanocodexToolMode {
    /// Returns the stable report spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CodeMode => "code_mode",
            Self::CodeModeOnly => "code_mode_only",
        }
    }

    const fn exposure(self) -> nanocodex_tools::ToolExposure {
        match self {
            Self::CodeMode => nanocodex_tools::ToolExposure::DirectAndCodeMode,
            Self::CodeModeOnly => nanocodex_tools::ToolExposure::CodeModeOnly,
        }
    }
}

/// A reusable recipe for matched Nanocodex-versus-Codex evaluations.
#[derive(Clone)]
pub struct DifferentialEvaluator {
    inner: Arc<DifferentialEvaluatorInner>,
}

struct DifferentialEvaluatorInner {
    nanocodex: NanocodexBuilder,
    codex_sha256: String,
    codex_release: Arc<DiffCodexRelease>,
    codex_auth: CodexAuth,
    vm: Arc<VmResources>,
    output: PathBuf,
    thinking: Thinking,
    web_search: bool,
    nanocodex_tool_mode: NanocodexToolMode,
    codex_tool_mode: CodexToolMode,
    nanocodex_build: ExecutableIdentity,
    admission: Arc<AdmissionController>,
    max_concurrency: usize,
    max_memory_mb: Option<u64>,
    max_infrastructure_replacements: usize,
    memory: Mutex<DifferentialMemoryPlanner>,
}

/// One semantic treatment in a centrally scheduled differential sweep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DifferentialProfile {
    thinking: Thinking,
    nanocodex_tool_mode: NanocodexToolMode,
    codex_tool_mode: CodexToolMode,
}

impl DifferentialProfile {
    /// Creates one reasoning-effort and paired tool-exposure treatment.
    #[must_use]
    pub const fn new(
        thinking: Thinking,
        nanocodex_tool_mode: NanocodexToolMode,
        codex_tool_mode: CodexToolMode,
    ) -> Self {
        Self {
            thinking,
            nanocodex_tool_mode,
            codex_tool_mode,
        }
    }

    /// Returns the reasoning effort shared by both arms.
    #[must_use]
    pub const fn thinking(self) -> Thinking {
        self.thinking
    }

    /// Returns Nanocodex's model-visible tool exposure.
    #[must_use]
    pub const fn nanocodex_tool_mode(self) -> NanocodexToolMode {
        self.nanocodex_tool_mode
    }

    /// Returns stock Codex's model-visible tool exposure.
    #[must_use]
    pub const fn codex_tool_mode(self) -> CodexToolMode {
        self.codex_tool_mode
    }

    fn name(self) -> String {
        format!(
            "{}__nanocodex_{}__codex_{}",
            self.thinking.as_str(),
            self.nanocodex_tool_mode.as_str(),
            self.codex_tool_mode.as_str()
        )
    }
}

#[derive(Clone)]
struct ScheduledComparison {
    task_index: usize,
    profile_index: usize,
    task: Task,
    trial: usize,
    profile: DifferentialProfile,
    infrastructure_replacement_for: Option<usize>,
    memory_attempt: usize,
    minimum_guest_memory_mb: Option<u64>,
    memory_retry_for: Option<PathBuf>,
    queued_at: DateTime<Utc>,
}

struct InfrastructureReplacementState {
    task: Task,
    profile: DifferentialProfile,
    next_trial: usize,
    remaining: usize,
    target_valid: usize,
    valid: usize,
    outstanding: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DifferentialMemoryPlan {
    guest_memory_mb: u64,
    nanocodex_admission_memory_mb: u64,
    codex_admission_memory_mb: u64,
}

impl DifferentialMemoryPlan {
    const fn pair_admission_memory_mb(self) -> u64 {
        self.nanocodex_admission_memory_mb
            .saturating_add(self.codex_admission_memory_mb)
    }
}

struct DifferentialMemoryPlanner {
    initial_guest_memory_mb: u64,
    path: PathBuf,
    profiles: DifferentialMemoryProfiles,
}

#[derive(Deserialize, Serialize)]
struct DifferentialMemoryProfiles {
    schema_version: u32,
    tasks: BTreeMap<String, DifferentialMemoryProfile>,
}

impl Default for DifferentialMemoryProfiles {
    fn default() -> Self {
        Self {
            schema_version: MEMORY_PROFILE_SCHEMA_VERSION,
            tasks: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct DifferentialMemoryProfile {
    task_name: String,
    content_digest: String,
    guest_memory_mb: u64,
    nanocodex_admission_memory_mb: u64,
    codex_admission_memory_mb: u64,
    oom_floor_guest_memory_mb: u64,
    nanocodex_host_peak_rss_mib: Option<u64>,
    codex_host_peak_rss_mib: Option<u64>,
    guest_peak_used_mib: Option<u64>,
    updated_at: DateTime<Utc>,
}

impl DifferentialMemoryPlanner {
    fn load(path: PathBuf, initial_guest_memory_mb: u64) -> InternalResult<Self> {
        let profiles = match fs::read(&path) {
            Ok(bytes) => {
                let profiles: DifferentialMemoryProfiles = serde_json::from_slice(&bytes)
                    .wrap_err_with(|| {
                        format!("failed to decode memory profiles {}", path.display())
                    })?;
                if profiles.schema_version != MEMORY_PROFILE_SCHEMA_VERSION {
                    return Err(diff_error!(
                        "memory profiles {} use schema {}; expected {}",
                        path.display(),
                        profiles.schema_version,
                        MEMORY_PROFILE_SCHEMA_VERSION
                    ));
                }
                profiles
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                DifferentialMemoryProfiles::default()
            }
            Err(error) => {
                return Err(Box::new(ContextError {
                    context: format!("failed to read memory profiles {}", path.display()),
                    source: Box::new(error),
                }));
            }
        };
        Ok(Self {
            initial_guest_memory_mb,
            path,
            profiles,
        })
    }

    fn plan(&self, task: &Task, minimum_guest_memory_mb: Option<u64>) -> DifferentialMemoryPlan {
        let declared_memory_mb = task.resources().memory_mb.max(1);
        let initial_guest_memory_mb = self.initial_guest_memory_mb.clamp(1, declared_memory_mb);
        let profile = self.profiles.tasks.get(task.content_digest());
        let learned_guest_memory_mb = profile
            .map_or(initial_guest_memory_mb, |profile| profile.guest_memory_mb)
            .clamp(1, declared_memory_mb);
        let guest_memory_mb = minimum_guest_memory_mb
            .map_or(learned_guest_memory_mb, |minimum| {
                learned_guest_memory_mb.max(minimum)
            })
            .clamp(1, declared_memory_mb);
        let uncalibrated_admission = guest_memory_mb;
        DifferentialMemoryPlan {
            guest_memory_mb,
            nanocodex_admission_memory_mb: profile.map_or(uncalibrated_admission, |profile| {
                profile.nanocodex_admission_memory_mb
            }),
            codex_admission_memory_mb: profile.map_or(uncalibrated_admission, |profile| {
                profile.codex_admission_memory_mb
            }),
        }
    }

    fn observe(&mut self, report: &DifferentialReport) -> InternalResult<()> {
        if report.oom_detected() {
            self.observe_oom(report);
        } else if report.is_memory_calibration_success() {
            self.observe_success(report);
        } else {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).wrap_err_with(|| {
                format!(
                    "failed to create memory profile directory {}",
                    parent.display()
                )
            })?;
        }
        write_json_atomic(&self.path, &self.profiles)
    }

    fn observe_oom(&mut self, report: &DifferentialReport) {
        let declared_memory_mb = report.declared_arm_memory_mb();
        let next_guest_memory_mb =
            next_guest_memory_after_oom(report.configured_guest_memory_mb(), declared_memory_mb)
                .unwrap_or(declared_memory_mb.max(1));
        let profile = self.profile_mut(report);
        profile.guest_memory_mb = profile.guest_memory_mb.max(next_guest_memory_mb);
        profile.oom_floor_guest_memory_mb =
            profile.oom_floor_guest_memory_mb.max(next_guest_memory_mb);
        profile.nanocodex_admission_memory_mb = profile
            .nanocodex_admission_memory_mb
            .max(next_guest_memory_mb);
        profile.codex_admission_memory_mb =
            profile.codex_admission_memory_mb.max(next_guest_memory_mb);
        profile.updated_at = Utc::now();
    }

    fn observe_success(&mut self, report: &DifferentialReport) {
        let declared_memory_mb = report.declared_arm_memory_mb();
        let configured_guest_memory_mb = report.configured_guest_memory_mb();
        let nanocodex_memory = report.nanocodex.memory.unwrap_or_default();
        let codex_memory = report.codex.memory.unwrap_or_default();
        let observed_guest_peak = max_optional_u64(
            nanocodex_memory.guest_peak_used_mib,
            codex_memory.guest_peak_used_mib,
        );
        let profile = self.profile_mut(report);
        profile.nanocodex_host_peak_rss_mib = max_optional_u64(
            profile.nanocodex_host_peak_rss_mib,
            nanocodex_memory.host_peak_rss_mib,
        );
        profile.codex_host_peak_rss_mib = max_optional_u64(
            profile.codex_host_peak_rss_mib,
            codex_memory.host_peak_rss_mib,
        );
        profile.guest_peak_used_mib =
            max_optional_u64(profile.guest_peak_used_mib, observed_guest_peak);
        let minimum_memory_mb = MINIMUM_DIFFERENTIAL_GUEST_MEMORY_MB.min(declared_memory_mb);
        profile.guest_memory_mb = profile
            .guest_peak_used_mib
            .map_or(configured_guest_memory_mb, memory_with_slack)
            .max(profile.oom_floor_guest_memory_mb)
            .clamp(minimum_memory_mb.max(1), declared_memory_mb);
        profile.nanocodex_admission_memory_mb = profile
            .nanocodex_host_peak_rss_mib
            .map_or(profile.guest_memory_mb, memory_with_slack)
            .max(1);
        profile.codex_admission_memory_mb = profile
            .codex_host_peak_rss_mib
            .map_or(profile.guest_memory_mb, memory_with_slack)
            .max(1);
        profile.updated_at = Utc::now();
    }

    fn profile_mut(&mut self, report: &DifferentialReport) -> &mut DifferentialMemoryProfile {
        self.profiles
            .tasks
            .entry(report.task.content_digest.clone())
            .or_insert_with(|| DifferentialMemoryProfile {
                task_name: report.task.name.clone(),
                content_digest: report.task.content_digest.clone(),
                guest_memory_mb: report.configured_guest_memory_mb(),
                nanocodex_admission_memory_mb: report.schedule.nanocodex_admission_memory_mb,
                codex_admission_memory_mb: report.schedule.codex_admission_memory_mb,
                oom_floor_guest_memory_mb: 0,
                nanocodex_host_peak_rss_mib: None,
                codex_host_peak_rss_mib: None,
                guest_peak_used_mib: None,
                updated_at: Utc::now(),
            })
    }
}

const fn memory_with_slack(memory_mb: u64) -> u64 {
    (memory_mb
        .saturating_mul(MEMORY_RECOMMENDATION_PERCENT)
        .saturating_add(99)
        / 100)
        .saturating_add(MEMORY_RECOMMENDATION_FIXED_SLACK_MB)
}

fn next_guest_memory_after_oom(current_mb: u64, declared_mb: u64) -> Option<u64> {
    let next_mb = current_mb.saturating_mul(2).min(declared_mb.max(1));
    if next_mb > current_mb {
        Some(next_mb)
    } else {
        None
    }
}

const fn max_optional_u64(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(if left > right { left } else { right }),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

struct DifferentialComparison {
    task: Task,
    trial: usize,
    nanocodex: NanocodexBuilder,
    codex_sha256: String,
    codex_release: Arc<DiffCodexRelease>,
    codex_auth: CodexAuth,
    vm: Arc<VmResources>,
    output: PathBuf,
    thinking: Thinking,
    web_search: bool,
    nanocodex_tool_mode: NanocodexToolMode,
    codex_tool_mode: CodexToolMode,
    nanocodex_build: ExecutableIdentity,
    schedule: DifferentialSchedule,
    memory_plan: DifferentialMemoryPlan,
    admission: AdmissionPermit,
}

/// Deliberate policy and required components for [`DifferentialEvaluator`].
pub struct DifferentialEvaluatorBuilder {
    nanocodex: NanocodexBuilder,
    codex: Option<(PathBuf, CodexAuth)>,
    vm: Option<Arc<VmResources>>,
    output: PathBuf,
    thinking: Thinking,
    web_search: bool,
    nanocodex_tool_mode: NanocodexToolMode,
    codex_tool_mode: CodexToolMode,
    nanocodex_build: Option<ExecutableIdentity>,
    max_concurrency: usize,
    max_memory_mb: Option<u64>,
    max_infrastructure_replacements: usize,
    initial_guest_memory_mb: u64,
    memory_profile_path: Option<PathBuf>,
}

/// Authentication material forwarded to a pinned stock-Codex guest.
#[derive(Clone)]
pub struct CodexAuth {
    kind: CodexAuthKind,
}

#[derive(Clone)]
enum CodexAuthKind {
    ApiKey(Arc<str>),
    AuthFile(PathBuf),
}

impl CodexAuth {
    /// Uses an OpenAI API key in the stock-Codex guest.
    #[must_use]
    pub fn api_key(api_key: impl Into<Arc<str>>) -> Self {
        Self {
            kind: CodexAuthKind::ApiKey(api_key.into()),
        }
    }

    /// Uses one Codex-compatible ChatGPT credential file in the guest.
    #[must_use]
    pub fn auth_file(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: CodexAuthKind::AuthFile(path.into()),
        }
    }
}

/// A pinned executable recorded in a differential report.
#[derive(Clone, Debug, Serialize)]
pub struct ExecutableIdentity {
    path: PathBuf,
    version: String,
    git_sha: Option<String>,
    built_at: Option<String>,
    sha256: String,
}

impl ExecutableIdentity {
    /// Creates identity metadata for an executable.
    ///
    /// The file digest is computed only when the differential run begins.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, version: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            version: version.into(),
            git_sha: None,
            built_at: None,
            sha256: String::new(),
        }
    }

    /// Records the source revision used to build the executable.
    #[must_use]
    pub fn git_sha(mut self, git_sha: impl Into<String>) -> Self {
        self.git_sha = Some(git_sha.into());
        self
    }

    /// Records the build timestamp supplied by the embedding application.
    #[must_use]
    pub fn built_at(mut self, built_at: impl Into<String>) -> Self {
        self.built_at = Some(built_at.into());
        self
    }

    fn resolve(mut self, label: &str) -> InternalResult<Self> {
        let (path, sha256) = resolve_executable(&self.path, label)?;
        self.path = path;
        self.sha256 = sha256;
        Ok(self)
    }
}

fn resolve_executable(path: &Path, label: &str) -> InternalResult<(PathBuf, String)> {
    let resolved = path
        .canonicalize()
        .wrap_err_with(|| format!("failed to resolve {label} executable {}", path.display()))?;
    if !resolved.is_file() {
        return Err(diff_error!(
            "{label} executable is not a regular file: {}",
            resolved.display()
        ));
    }
    let sha256 = file_sha256(&resolved)?;
    Ok((resolved, sha256))
}

/// Missing required component while building a differential evaluation.
#[derive(Debug, thiserror::Error)]
pub enum DifferentialBuildError {
    /// No pinned stock-Codex executable and auth were supplied.
    #[error("a differential evaluation requires a stock-Codex executable and auth")]
    MissingCodex,

    /// No prepared VM resource set was supplied.
    #[error("a differential evaluation requires prepared VM resources")]
    MissingVm,

    /// No Nanocodex executable identity was supplied.
    #[error("a differential evaluation requires Nanocodex executable identity")]
    MissingNanocodexIdentity,

    /// The configured pair concurrency was zero.
    #[error("differential pair concurrency must be greater than zero")]
    InvalidConcurrency,

    /// The configured measured host-memory target was zero.
    #[error("differential host-memory target must be greater than zero")]
    InvalidMemory,

    /// The configured initial per-arm guest memory was zero.
    #[error("differential initial guest memory must be greater than zero")]
    InvalidInitialGuestMemory,

    /// A pinned executable could not be resolved or hashed.
    #[error("failed to prepare differential executable identity: {0}")]
    Executable(#[source] DifferentialError),

    /// Shared stock-Codex guest assets could not be staged.
    #[error("failed to prepare shared stock-Codex guest assets: {0}")]
    Assets(#[source] DifferentialError),

    /// Retained adaptive memory profiles could not be loaded safely.
    #[error("failed to load differential memory profiles: {0}")]
    MemoryProfiles(#[source] DifferentialError),

    /// The blocking asset-preparation task did not complete.
    #[error("differential asset preparation task failed: {0}")]
    PreparationTask(#[from] tokio::task::JoinError),
}

/// Runtime or retained-evidence failure in a differential evaluation.
#[derive(Debug)]
pub struct DifferentialError {
    source: BoxError,
}

impl DifferentialError {
    fn new(source: BoxError) -> Self {
        Self { source }
    }
}

impl Display for DifferentialError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.source, formatter)
    }
}

impl Error for DifferentialError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Result returned by differential execution and retained-evidence analysis.
pub type DifferentialResult<T> = std::result::Result<T, DifferentialError>;

#[derive(Serialize)]
/// Complete retained outcome and evidence index for one paired run.
pub struct DifferentialReport {
    schema_version: u32,
    id: Uuid,
    task: TaskIdentity,
    trial: usize,
    model: String,
    thinking: String,
    policy: ComparisonPolicy,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    duration_ms: u64,
    schedule: DifferentialSchedule,
    classification: DifferentialClassification,
    trajectory_comparison: TrajectoryComparison,
    api_comparison: ApiComparisonSummary,
    nanocodex_build: ExecutableIdentity,
    codex_build: ExecutableIdentity,
    nanocodex: ArmReport,
    codex: ArmReport,
    artifacts: ComparisonArtifacts,
}

/// Durable result of one centrally scheduled differential sweep.
#[derive(Serialize)]
pub struct DifferentialSweepResults {
    reports: Vec<DifferentialReport>,
    summaries: Vec<DifferentialReportSummary>,
    skipped: usize,
}

/// Small stable index entry for either a newly completed or resumed pair.
#[derive(Clone, Serialize)]
pub struct DifferentialReportSummary {
    task_name: String,
    task_root: PathBuf,
    task_content_digest: String,
    trial: usize,
    thinking: String,
    nanocodex_tool_mode: NanocodexToolMode,
    codex_tool_mode: CodexToolMode,
    classification: DifferentialClassification,
    infrastructure_failure: bool,
    operational_error: bool,
    oom_detected: bool,
    memory_attempt: usize,
    configured_guest_memory_mb: u64,
    declared_guest_memory_mb: u64,
    infrastructure_replacement_for: Option<usize>,
    comparison_path: PathBuf,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct DifferentialSweepManifest {
    schema_version: u32,
    comparison_schema_version: u32,
    model: String,
    web_search: bool,
    trials: usize,
    tasks: Vec<DifferentialSweepTask>,
    profiles: Vec<DifferentialSweepProfile>,
    nanocodex_sha256: String,
    codex_sha256: String,
}

#[derive(Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct DifferentialSweepTask {
    name: String,
    root: PathBuf,
    content_digest: String,
}

#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct DifferentialSweepProfile {
    thinking: String,
    nanocodex_tool_mode: String,
    codex_tool_mode: String,
}

struct DifferentialSweepGuard {
    _lock: File,
}

#[derive(Deserialize)]
struct RetainedDifferentialReport {
    schema_version: u32,
    task: RetainedTaskIdentity,
    trial: usize,
    model: String,
    thinking: String,
    policy: RetainedComparisonPolicy,
    schedule: DifferentialSchedule,
    classification: DifferentialClassification,
    nanocodex_build: RetainedExecutableIdentity,
    codex_build: RetainedExecutableIdentity,
    nanocodex: RetainedArmReport,
    codex: RetainedArmReport,
    artifacts: RetainedComparisonArtifacts,
}

#[derive(Deserialize)]
struct RetainedTaskIdentity {
    name: String,
    root: PathBuf,
    content_digest: String,
}

#[derive(Deserialize)]
struct RetainedComparisonPolicy {
    web_search: bool,
    #[serde(default)]
    nanocodex_tool_mode: NanocodexToolMode,
    codex_tool_mode: CodexToolMode,
}

#[derive(Deserialize)]
struct RetainedExecutableIdentity {
    sha256: String,
}

#[derive(Deserialize)]
struct RetainedArmReport {
    operational_error: Option<String>,
    event_error: Option<String>,
    trajectory_error: Option<String>,
    api_capture_error: Option<String>,
    memory: Option<ArmMemoryReport>,
    outcome: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RetainedComparisonArtifacts {
    comparison: PathBuf,
    progress_error: Option<String>,
    api_comparison_error: Option<String>,
    profile_validation_error: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct DifferentialSchedule {
    queued_at: DateTime<Utc>,
    admitted_at: DateTime<Utc>,
    queue_duration_ms: u64,
    declared_pair_memory_mb: u64,
    requested_pair_memory_mb: u64,
    admitted_pair_memory_mb: u64,
    configured_guest_memory_mb: u64,
    nanocodex_admission_memory_mb: u64,
    codex_admission_memory_mb: u64,
    memory_attempt: usize,
    memory_retry_for: Option<PathBuf>,
    max_concurrency: usize,
    max_memory_mb: Option<u64>,
    max_infrastructure_replacements: usize,
    infrastructure_replacement_for: Option<usize>,
}

const fn differential_pair_memory_mb(arm_memory_mb: u64) -> u64 {
    arm_memory_mb.saturating_mul(2)
}

fn releasable_differential_arm_memory_mb(
    arm_memory_mb: u64,
    pair_memory_mb: u64,
    max_memory_mb: Option<u64>,
) -> u64 {
    if max_memory_mb.is_some_and(|limit| pair_memory_mb <= limit) {
        arm_memory_mb
    } else {
        0
    }
}

fn differential_comparison_name(
    task: &Task,
    profile: DifferentialProfile,
    trial: usize,
    id: Uuid,
) -> String {
    let short_name = task.name().rsplit('/').next().unwrap_or(task.name());
    format!(
        "{short_name}__{}__{trial:03}__{}",
        profile.name(),
        id.simple()
    )
}

fn release_differential_arm_memory(
    admission: &mut AdmissionPermit,
    arm_memory_mb: u64,
    arm: &'static str,
) {
    let (released_slots, released_mb) = admission.release(1, arm_memory_mb);
    if released_slots > 0 || released_mb > 0 {
        info!(
            comparison_arm = arm,
            scheduler.concurrency.released = released_slots,
            scheduler.memory.released_mb = released_mb,
            "released completed differential arm capacity"
        );
    }
}

async fn join_differential_arms<N, C>(
    mut admission: AdmissionPermit,
    nanocodex_memory_mb: u64,
    codex_memory_mb: u64,
    nanocodex: N,
    codex: C,
) -> (N::Output, C::Output)
where
    N: Future,
    C: Future,
{
    tokio::pin!(nanocodex);
    tokio::pin!(codex);
    tokio::select! {
        nanocodex_result = &mut nanocodex => {
            release_differential_arm_memory(&mut admission, nanocodex_memory_mb, "nanocodex");
            let codex_result = codex.await;
            release_differential_arm_memory(&mut admission, codex_memory_mb, "codex");
            (nanocodex_result, codex_result)
        }
        codex_result = &mut codex => {
            release_differential_arm_memory(&mut admission, codex_memory_mb, "codex");
            let nanocodex_result = nanocodex.await;
            release_differential_arm_memory(&mut admission, nanocodex_memory_mb, "nanocodex");
            (nanocodex_result, codex_result)
        }
    }
}

/// Result of rebuilding derived trajectory and API comparisons from retained evidence.
#[derive(Serialize)]
pub struct DifferentialReanalysis {
    comparison: serde_json::Value,
    comparison_path: PathBuf,
    api_comparison_path: Option<PathBuf>,
    #[serde(skip)]
    human_summary: String,
}

#[derive(Serialize)]
struct TaskIdentity {
    name: String,
    root: PathBuf,
    content_digest: String,
}

#[derive(Serialize)]
struct ComparisonPolicy {
    runner: &'static str,
    environment: &'static str,
    attempts_per_agent: u8,
    execution_mode: &'static str,
    web_search: bool,
    codex_ephemeral: bool,
    codex_approval_policy: &'static str,
    codex_sandbox: &'static str,
    nanocodex_tool_mode: NanocodexToolMode,
    codex_tool_mode: CodexToolMode,
    multi_agent: &'static str,
    reasoning_summary: &'static str,
    expected_nanocodex_visible_tools: Vec<&'static str>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
/// Outcome relationship between the two matched verifier results.
pub enum DifferentialClassification {
    /// Both agents passed the verifier.
    BothPassed,
    /// Only stock Codex passed the verifier.
    CodexOnlyPassed,
    /// Only Nanocodex passed the verifier.
    NanocodexOnlyPassed,
    /// Both agents completed without passing the verifier.
    NeitherPassed,
    /// At least one runner or derived evidence path failed operationally.
    Incomplete,
}

#[derive(Serialize)]
struct ArmReport {
    summary: ArmSummary,
    evaluator_directory: Option<PathBuf>,
    event_log: Option<PathBuf>,
    trajectory: Option<PathBuf>,
    trajectory_summary: Option<TrajectorySummary>,
    trajectory_error: Option<String>,
    api_exchanges: Option<PathBuf>,
    api_capture: Option<ApiCaptureSummary>,
    api_capture_error: Option<String>,
    codex_events: Option<PathBuf>,
    codex_stderr: Option<PathBuf>,
    codex_summary: Option<PathBuf>,
    operational_error: Option<String>,
    event_error: Option<String>,
    memory: Option<ArmMemoryReport>,
    outcome: Option<EvalAttemptOutcome>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct ArmMemoryReport {
    host_peak_rss_mib: Option<u64>,
    guest_total_mib: Option<u64>,
    guest_peak_used_mib: Option<u64>,
    guest_oom_kills: u64,
    oom_detected: bool,
}

#[derive(Serialize)]
struct TrajectorySummary {
    total_steps: u32,
    agent_steps: u32,
    message_steps: u32,
    reasoning_steps: u32,
    tool_calls: u32,
    observations: u32,
    model_calls: Option<u32>,
    tool_projection: &'static str,
    tool_sequence: Vec<String>,
    shell_polling: ShellPollingSummary,
    usage_completeness: Option<MeasurementCompleteness>,
    runtime_completeness: MeasurementCompleteness,
}

#[derive(Serialize)]
struct ShellPollingSummary {
    poll_only_steps: u32,
    model_call_attribution_complete: bool,
    confirmed_model_calls: Option<u32>,
    empty_stdin_tool_calls: u32,
    sessions: u32,
    explicit_requested_yield_ms: u64,
    tool_wait_duration_ns: u64,
    model_duration_ns: u64,
    prompt_tokens: u64,
    cached_tokens: u64,
    completion_tokens: u64,
}

#[derive(Serialize)]
struct TrajectoryComparison {
    comparable: bool,
    tool_sequence_comparable: bool,
    tool_sequence_equal: Option<bool>,
    codex_minus_nanocodex: Option<TrajectoryDelta>,
}

#[derive(Serialize)]
struct TrajectoryDelta {
    total_steps: i64,
    agent_steps: i64,
    message_steps: i64,
    reasoning_steps: i64,
    tool_calls: Option<i64>,
    observations: Option<i64>,
    model_calls: Option<i64>,
    shell_polling: ShellPollingDelta,
}

#[derive(Serialize)]
struct ShellPollingDelta {
    poll_only_steps: i64,
    confirmed_model_calls: Option<i64>,
    empty_stdin_tool_calls: i64,
    sessions: i64,
    explicit_requested_yield_ms: i64,
    tool_wait_duration_ns: i64,
    model_duration_ns: i64,
    prompt_tokens: i64,
    cached_tokens: i64,
    completion_tokens: i64,
}

enum TrajectoryProjection {
    Nanocodex,
    Codex { version: CodexVersion },
}

enum CodexVersion {
    #[cfg(test)]
    Fixed(String),
    Guest(Arc<OnceLock<String>>),
}

impl CodexVersion {
    fn resolve(&self) -> InternalResult<String> {
        match self {
            #[cfg(test)]
            Self::Fixed(version) => Ok(version.clone()),
            Self::Guest(version) => version.get().cloned().ok_or_else(|| {
                diff_error!("stock Codex did not report its version inside the guest")
            }),
        }
    }
}

struct EventRecording {
    atif: AtifBuilder,
    atif_error: Option<String>,
}

struct TrajectoryArtifact {
    path: PathBuf,
    summary: TrajectorySummary,
}

#[derive(Serialize)]
struct ArmSummary {
    status: ArmStatus,
    outcome: Option<EvalOutcome>,
    exception: Option<EvalExceptionKind>,
    verifier_exit_code: Option<i32>,
    rewards: BTreeMap<String, f64>,
    model: Option<String>,
    tool_calls: Option<u64>,
    tool_call_measurement: &'static str,
    observed_tool_events: Option<u32>,
    usage: Option<UsageTotals>,
    duration_ms: Option<u64>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ArmStatus {
    Passed,
    VerifierFailed,
    Unscored,
    RunnerError,
}

#[derive(Serialize)]
struct ComparisonArtifacts {
    directory: PathBuf,
    comparison: PathBuf,
    progress: PathBuf,
    progress_error: Option<String>,
    api_comparison: Option<PathBuf>,
    api_comparison_error: Option<String>,
    profile_validation_error: Option<String>,
}

#[derive(Clone, Serialize)]
struct ApiCaptureSummary {
    schema_version: u32,
    payload_scope: &'static str,
    header_scope: &'static str,
    payload_fidelity: &'static str,
    records: u64,
    requests: u64,
    response_requests: u64,
    auxiliary_requests: u64,
    inbound_events: u64,
    terminal_events: u64,
    http_responses_completed: u64,
    payload_bytes: u64,
    exchange_complete: bool,
    transports: BTreeMap<String, u64>,
    phases: BTreeMap<String, u64>,
}

#[derive(Serialize)]
struct ApiComparisonReport {
    schema_version: u32,
    comparable: bool,
    request_count_equal: Option<bool>,
    aligned_requests: u64,
    nanocodex_unpaired_requests: u64,
    codex_unpaired_requests: u64,
    equal_requests: u64,
    differing_requests: u64,
    nanocodex: Option<ApiCaptureSummary>,
    codex: Option<ApiCaptureSummary>,
    first_divergence: Option<ApiFirstDivergence>,
    event_loop: ApiEventLoopComparison,
    requests: Vec<ApiRequestComparison>,
}

#[derive(Clone, Serialize)]
struct ApiComparisonSummary {
    comparable: bool,
    request_count_equal: Option<bool>,
    aligned_requests: u64,
    nanocodex_unpaired_requests: u64,
    codex_unpaired_requests: u64,
    equal_requests: u64,
    differing_requests: u64,
    first_divergence: Option<ApiFirstDivergence>,
    event_loop: ApiEventLoopComparison,
}

#[derive(Clone, Serialize)]
struct ApiFirstDivergence {
    request_index: u64,
    pointer: String,
}

#[derive(Serialize)]
struct ApiRequestComparison {
    request_index: u64,
    nanocodex_request_index: Option<u64>,
    codex_request_index: Option<u64>,
    nanocodex_phase: Option<String>,
    codex_phase: Option<String>,
    equal: bool,
    nanocodex_sha256: Option<String>,
    codex_sha256: Option<String>,
    differences: Vec<ApiJsonDifference>,
    event_loop: ApiEventLoopTurnComparison,
}

#[derive(Clone, Serialize)]
struct ApiEventLoopComparison {
    comparable: bool,
    request_count_equal: Option<bool>,
    chain_invariants_equal: Option<bool>,
    model_visible_tool_sequence_equal: Option<bool>,
    initial_input_text_sections_equal: Option<bool>,
    initial_generation_input_text_sections_equal: Option<bool>,
    initial_visible_tool_definitions_equal: Option<bool>,
    initial_generation_visible_tool_definitions_equal: Option<bool>,
    initial_code_mode_tool_names_equal: Option<bool>,
    initial_code_mode_tool_definitions_equal: Option<bool>,
    aligned_turns: u64,
    nanocodex_unpaired_turns: u64,
    codex_unpaired_turns: u64,
    equal_turns: u64,
    differing_turns: u64,
    first_divergence: Option<ApiEventLoopFirstDivergence>,
    first_generation_divergence: Option<ApiEventLoopFirstDivergence>,
    nanocodex_unpaired_tail: Option<ApiEventLoopTailSummary>,
    codex_unpaired_tail: Option<ApiEventLoopTailSummary>,
    nanocodex: Option<ApiEventLoopArmSummary>,
    codex: Option<ApiEventLoopArmSummary>,
}

#[derive(Clone, Serialize)]
struct ApiEventLoopFirstDivergence {
    request_index: u64,
    pointer: String,
    categories: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct ApiEventLoopTailSummary {
    turns: u64,
    generation_turns: u64,
    tool_call_turns: u64,
    detected_poll_only_turns: u64,
    detected_empty_stdin_calls: u64,
    detected_polling_calls_with_explicit_yield: u64,
    detected_polling_explicit_yield_ms: u64,
    turns_with_usage: u64,
    turns_without_usage: u64,
    usage: ApiTokenUsageSummary,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct ApiTokenUsageSummary {
    input_tokens: u64,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
struct ApiEventLoopArmSummary {
    turns: u64,
    generation_turns: u64,
    terminal_turns: u64,
    turns_with_usage: u64,
    turns_without_usage: u64,
    usage: ApiTokenUsageSummary,
    tool_call_turns: u64,
    model_visible_tool_calls: u64,
    model_visible_tool_sequence: Vec<String>,
    initial_model: Option<String>,
    initial_reasoning_effort: Option<String>,
    initial_reasoning_summary: Option<String>,
    initial_visible_tools: Vec<String>,
    initial_input_text_sections: Vec<ApiInputTextSectionSummary>,
    initial_generation_input_text_sections: Vec<ApiInputTextSectionSummary>,
    initial_visible_tool_definitions: Vec<ApiVisibleToolDefinitionSummary>,
    initial_generation_visible_tool_definitions: Vec<ApiVisibleToolDefinitionSummary>,
    initial_code_mode_tools: Option<Vec<String>>,
    initial_code_mode_tool_definitions: Option<Vec<ApiCodeModeToolDefinitionSummary>>,
    detected_poll_only_turns: u64,
    max_consecutive_detected_poll_only_turns: u64,
    detected_empty_stdin_calls: u64,
    detected_polling_calls_with_explicit_yield: u64,
    detected_polling_explicit_yield_ms: u64,
    detected_poll_only_input_tokens: u64,
    detected_poll_only_cached_tokens: u64,
    detected_poll_only_output_tokens: u64,
    prompt_cache_key_stable: Option<bool>,
    previous_response_links: u64,
    full_history_replays: u64,
    full_history_replays_after_nonterminal_turn: u64,
    broken_previous_response_links: u64,
    tool_result_links: u64,
    replayed_tool_result_links: u64,
    broken_tool_result_links: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ApiCodeModeToolDefinitionSummary {
    name: String,
    ordinal: u64,
    section_bytes: u64,
    section_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ApiVisibleToolDefinitionSummary {
    name: String,
    ordinal: u64,
    description_bytes: Option<u64>,
    description_sha256: Option<String>,
    definition_bytes: u64,
    definition_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ApiInputTextSectionSummary {
    item_ordinal: u64,
    content_ordinal: u64,
    role: String,
    label: String,
    text_bytes: u64,
    text_sha256: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct DetectedPollingTurn {
    empty_stdin_calls: u64,
    calls_with_explicit_yield: u64,
    explicit_requested_yield_ms: u64,
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DetectedEmptyStdinCalls {
    calls: u64,
    calls_with_explicit_yield: u64,
    explicit_requested_yield_ms: u64,
}

impl ApiEventLoopArmSummary {
    fn chain_invariants_equal(&self, other: &Self) -> bool {
        self.turns == other.turns
            && self.generation_turns == other.generation_turns
            && self.terminal_turns == other.terminal_turns
            && self.prompt_cache_key_stable == other.prompt_cache_key_stable
            && self.previous_response_links == other.previous_response_links
            && self.full_history_replays == other.full_history_replays
            && self.full_history_replays_after_nonterminal_turn
                == other.full_history_replays_after_nonterminal_turn
            && self.broken_previous_response_links == other.broken_previous_response_links
            && self.tool_result_links == other.tool_result_links
            && self.replayed_tool_result_links == other.replayed_tool_result_links
            && self.broken_tool_result_links == other.broken_tool_result_links
    }
}

impl ApiEventLoopTrace {
    fn unpaired_tail(&self, aligned_turns: usize) -> ApiEventLoopTailSummary {
        ApiEventLoopTailSummary::from_turns(
            self.turn_metrics.get(aligned_turns..).unwrap_or_default(),
        )
    }
}

impl ApiEventLoopTailSummary {
    fn from_turns(turns: &[ApiEventLoopTurnMetrics]) -> Self {
        let mut summary = Self {
            turns: u64::try_from(turns.len()).unwrap_or(u64::MAX),
            ..Self::default()
        };
        for turn in turns {
            if turn.generation {
                summary.generation_turns = summary.generation_turns.saturating_add(1);
            }
            if turn.tool_calls > 0 {
                summary.tool_call_turns = summary.tool_call_turns.saturating_add(1);
            }
            if let Some(polling) = &turn.detected_polling {
                summary.detected_poll_only_turns =
                    summary.detected_poll_only_turns.saturating_add(1);
                summary.detected_empty_stdin_calls = summary
                    .detected_empty_stdin_calls
                    .saturating_add(polling.empty_stdin_calls);
                summary.detected_polling_calls_with_explicit_yield = summary
                    .detected_polling_calls_with_explicit_yield
                    .saturating_add(polling.calls_with_explicit_yield);
                summary.detected_polling_explicit_yield_ms = summary
                    .detected_polling_explicit_yield_ms
                    .saturating_add(polling.explicit_requested_yield_ms);
            }
            if let Some(usage) = &turn.usage {
                summary.turns_with_usage = summary.turns_with_usage.saturating_add(1);
                summary.usage.add(usage);
            } else {
                summary.turns_without_usage = summary.turns_without_usage.saturating_add(1);
            }
        }
        summary
    }
}

impl ApiTokenUsageSummary {
    const fn add(&mut self, usage: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.uncached_input_tokens = self
            .uncached_input_tokens
            .saturating_add(usage.uncached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(usage.total_tokens);
    }
}

#[derive(Serialize)]
struct ApiEventLoopTurnComparison {
    equal: bool,
    categories: Vec<String>,
    nanocodex: Option<serde_json::Value>,
    codex: Option<serde_json::Value>,
    differences: Vec<ApiJsonDifference>,
}

#[derive(Serialize)]
struct ApiJsonDifference {
    pointer: String,
    nanocodex: ApiJsonSide,
    codex: ApiJsonSide,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum ApiJsonSide {
    Missing,
    Value { value: serde_json::Value },
}

struct ApiCaptureArtifact {
    path: PathBuf,
    summary: ApiCaptureSummary,
}

struct ApiRequestPayload {
    request_index: u64,
    phase: Option<String>,
    payload: serde_json::Value,
    sha256: String,
    response_events: Vec<serde_json::Value>,
}

struct ApiEventLoopTrace {
    turns: Vec<serde_json::Value>,
    turn_metrics: Vec<ApiEventLoopTurnMetrics>,
    summary: ApiEventLoopArmSummary,
}

struct ApiEventLoopTurnMetrics {
    generation: bool,
    tool_calls: u64,
    detected_polling: Option<DetectedPollingTurn>,
    usage: Option<ApiTokenUsageSummary>,
}

mod progress;
use progress::*;

mod codex_vm;
use codex_vm::*;

fn differential_sweep_manifest(
    inner: &DifferentialEvaluatorInner,
    tasks: &[Task],
    profiles: &[DifferentialProfile],
    trials: usize,
) -> DifferentialSweepManifest {
    let mut tasks = tasks
        .iter()
        .map(|task| DifferentialSweepTask {
            name: task.name().to_owned(),
            root: task.root().to_path_buf(),
            content_digest: task.content_digest().to_owned(),
        })
        .collect::<Vec<_>>();
    tasks.sort_unstable();
    let profiles = differential_sweep_profiles(profiles);
    DifferentialSweepManifest {
        schema_version: SWEEP_MANIFEST_SCHEMA_VERSION,
        comparison_schema_version: COMPARISON_SCHEMA_VERSION,
        model: MODEL.to_owned(),
        web_search: inner.web_search,
        trials,
        tasks,
        profiles,
        nanocodex_sha256: inner.nanocodex_build.sha256.clone(),
        codex_sha256: inner.codex_sha256.clone(),
    }
}

fn differential_sweep_profiles(profiles: &[DifferentialProfile]) -> Vec<DifferentialSweepProfile> {
    profiles
        .iter()
        .map(|profile| DifferentialSweepProfile {
            thinking: profile.thinking.as_str().to_owned(),
            nanocodex_tool_mode: profile.nanocodex_tool_mode.as_str().to_owned(),
            codex_tool_mode: profile.codex_tool_mode.as_str().to_owned(),
        })
        .collect()
}

fn prepare_differential_sweep(
    inner: &DifferentialEvaluatorInner,
    tasks: &[Task],
    profiles: &[DifferentialProfile],
    trials: usize,
) -> InternalResult<(DifferentialSweepGuard, Vec<DifferentialReportSummary>)> {
    let lock_path = inner.output.join(SWEEP_LOCK_FILE);
    let lock = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .wrap_err_with(|| format!("failed to open sweep lock {}", lock_path.display()))?;
    lock.try_lock_exclusive().map_err(|error| {
        diff_error!(
            "another differential runner owns {}: {error}",
            lock_path.display()
        )
    })?;
    let guard = DifferentialSweepGuard { _lock: lock };
    let expected = differential_sweep_manifest(inner, tasks, profiles, trials);
    let manifest_path = inner.output.join(SWEEP_MANIFEST_FILE);
    if manifest_path.is_file() {
        let bytes = fs::read(&manifest_path).wrap_err_with(|| {
            format!(
                "failed to read differential sweep manifest {}",
                manifest_path.display()
            )
        })?;
        let retained: DifferentialSweepManifest =
            serde_json::from_slice(&bytes).wrap_err_with(|| {
                format!(
                    "failed to decode differential sweep manifest {}",
                    manifest_path.display()
                )
            })?;
        if retained != expected {
            return Err(diff_error!(
                "differential sweep manifest {} does not match the requested tasks, profiles, \
                 trials, model, or executable builds; choose a new --output directory",
                manifest_path.display()
            ));
        }
    } else {
        write_json_atomic(&manifest_path, &expected).map_err(|source| {
            Box::new(ContextError {
                context: format!(
                    "failed to retain differential sweep manifest {}",
                    manifest_path.display()
                ),
                source,
            }) as BoxError
        })?;
    }
    let removed_upper_disks = cleanup_incomplete_differential_upper_disks(&inner.output)?;
    if removed_upper_disks > 0 {
        info!(
            removed_upper_disks,
            output = %inner.output.display(),
            "removed writable VM disks left by interrupted differential comparisons"
        );
    }
    let summaries = scan_differential_reports(inner, &expected)?;
    Ok((guard, summaries))
}

fn cleanup_incomplete_differential_upper_disks(output: &Path) -> InternalResult<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(output).wrap_err_with(|| {
        format!(
            "failed to scan differential sweep output {} for interrupted comparisons",
            output.display()
        )
    })? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let comparison_directory = entry.path();
        if comparison_directory.join(COMPARISON_FILE).is_file()
            || !comparison_directory.join(PROGRESS_FILE).is_file()
        {
            continue;
        }

        let mut directories = vec![comparison_directory];
        while let Some(directory) = directories.pop() {
            for child in fs::read_dir(&directory).wrap_err_with(|| {
                format!(
                    "failed to scan interrupted comparison directory {}",
                    directory.display()
                )
            })? {
                let child = child?;
                let file_type = child.file_type()?;
                if file_type.is_dir() {
                    directories.push(child.path());
                } else if file_type.is_file()
                    && child
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.ends_with(DIFFERENTIAL_UPPER_DISK_SUFFIX))
                {
                    let path = child.path();
                    fs::remove_file(&path).wrap_err_with(|| {
                        format!(
                            "failed to remove writable VM disk left by interrupted comparison {}",
                            path.display()
                        )
                    })?;
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

fn scan_differential_reports(
    inner: &DifferentialEvaluatorInner,
    manifest: &DifferentialSweepManifest,
) -> InternalResult<Vec<DifferentialReportSummary>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(&inner.output).wrap_err_with(|| {
        format!(
            "failed to scan differential sweep output {}",
            inner.output.display()
        )
    })? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = entry.path().join(COMPARISON_FILE);
            if path.is_file() {
                paths.push(path);
            }
        }
    }
    paths.sort_unstable();
    paths
        .into_iter()
        .map(|path| retained_differential_summary(&path, manifest))
        .collect()
}

fn retained_differential_summary(
    path: &Path,
    manifest: &DifferentialSweepManifest,
) -> InternalResult<DifferentialReportSummary> {
    let bytes = fs::read(path)
        .wrap_err_with(|| format!("failed to read retained comparison {}", path.display()))?;
    let report: RetainedDifferentialReport = serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("failed to decode retained comparison {}", path.display()))?;
    if report.schema_version != manifest.comparison_schema_version {
        return Err(diff_error!(
            "retained comparison {} uses schema {}; expected {}",
            path.display(),
            report.schema_version,
            manifest.comparison_schema_version
        ));
    }
    let task_matches = manifest.tasks.iter().any(|task| {
        task.name == report.task.name
            && task.root == report.task.root
            && task.content_digest == report.task.content_digest
    });
    let profile_matches = manifest.profiles.iter().any(|profile| {
        profile.thinking == report.thinking
            && profile.nanocodex_tool_mode == report.policy.nanocodex_tool_mode.as_str()
            && profile.codex_tool_mode == report.policy.codex_tool_mode.as_str()
    });
    if !task_matches
        || !profile_matches
        || report.model != manifest.model
        || report.policy.web_search != manifest.web_search
        || report.nanocodex_build.sha256 != manifest.nanocodex_sha256
        || report.codex_build.sha256 != manifest.codex_sha256
    {
        return Err(diff_error!(
            "retained comparison {} does not belong to its differential sweep manifest",
            path.display()
        ));
    }
    if report.artifacts.comparison != path {
        return Err(diff_error!(
            "retained comparison {} records a different comparison path {}",
            path.display(),
            report.artifacts.comparison.display()
        ));
    }
    let oom_detected = [&report.nanocodex, &report.codex]
        .into_iter()
        .any(|arm| arm.memory.is_some_and(|memory| memory.oom_detected));
    let infrastructure_failure = oom_detected
        || [&report.nanocodex, &report.codex]
            .into_iter()
            .any(retained_arm_has_infrastructure_failure);
    let operational_error = report.artifacts.progress_error.is_some()
        || report.artifacts.api_comparison_error.is_some()
        || report.artifacts.profile_validation_error.is_some()
        || [&report.nanocodex, &report.codex]
            .into_iter()
            .any(retained_arm_has_operational_error);
    Ok(DifferentialReportSummary {
        task_name: report.task.name,
        task_root: report.task.root,
        task_content_digest: report.task.content_digest,
        trial: report.trial,
        thinking: report.thinking,
        nanocodex_tool_mode: report.policy.nanocodex_tool_mode,
        codex_tool_mode: report.policy.codex_tool_mode,
        classification: report.classification,
        infrastructure_failure,
        operational_error,
        oom_detected,
        memory_attempt: report.schedule.memory_attempt,
        configured_guest_memory_mb: report.schedule.configured_guest_memory_mb,
        declared_guest_memory_mb: report.schedule.declared_pair_memory_mb / 2,
        infrastructure_replacement_for: report.schedule.infrastructure_replacement_for,
        comparison_path: path.to_path_buf(),
    })
}

fn retained_arm_has_infrastructure_failure(arm: &RetainedArmReport) -> bool {
    arm.outcome
        .as_ref()
        .and_then(|outcome| outcome.pointer("/attempt/outcome"))
        .and_then(serde_json::Value::as_str)
        == Some("infrastructure_error")
}

const fn retained_arm_has_operational_error(arm: &RetainedArmReport) -> bool {
    arm.operational_error.is_some()
        || arm.event_error.is_some()
        || arm.trajectory_error.is_some()
        || arm.api_capture_error.is_some()
}

fn resume_differential_schedule(
    pending: &mut VecDeque<ScheduledComparison>,
    replacements: &mut [InfrastructureReplacementState],
    summaries: &[DifferentialReportSummary],
    requested_trials: usize,
    max_infrastructure_replacements: usize,
    profile_count: usize,
) -> usize {
    let mut retained_pending = VecDeque::with_capacity(pending.len());
    let mut skipped = 0_usize;
    while let Some(scheduled) = pending.pop_front() {
        let matching_trial = summaries
            .iter()
            .filter(|summary| {
                summary.matches(&scheduled.task, scheduled.profile)
                    && summary.trial == scheduled.trial
            })
            .collect::<Vec<_>>();
        if matching_trial.iter().any(|summary| summary.is_valid()) {
            skipped = skipped.saturating_add(1);
            continue;
        }
        let latest = matching_trial.iter().copied().max_by(|left, right| {
            (left.memory_attempt, &left.comparison_path)
                .cmp(&(right.memory_attempt, &right.comparison_path))
        });
        match latest {
            None => retained_pending.push_back(scheduled),
            Some(summary)
                if summary.oom_detected
                    && summary.configured_guest_memory_mb < summary.declared_guest_memory_mb =>
            {
                let mut scheduled = scheduled;
                scheduled.memory_attempt = summary.memory_attempt.saturating_add(1);
                scheduled.minimum_guest_memory_mb = Some(
                    summary
                        .configured_guest_memory_mb
                        .saturating_mul(2)
                        .min(summary.declared_guest_memory_mb),
                );
                scheduled.memory_retry_for = Some(summary.comparison_path.clone());
                retained_pending.push_back(scheduled);
            }
            Some(_) => {}
        }
    }
    *pending = retained_pending;

    for (replacement_index, replacement) in replacements.iter_mut().enumerate() {
        let task_index = replacement_index / profile_count;
        let profile_index = replacement_index % profile_count;
        let matching = summaries
            .iter()
            .filter(|summary| summary.matches(&replacement.task, replacement.profile))
            .collect::<Vec<_>>();
        let mut max_trial = requested_trials;
        let mut replacement_trials = BTreeSet::new();
        let mut linked_failures = BTreeSet::new();
        let mut valid_trials = BTreeSet::new();
        let mut latest_by_trial = BTreeMap::<usize, &DifferentialReportSummary>::new();
        for &summary in &matching {
            max_trial = max_trial.max(summary.trial);
            if summary.is_valid() {
                valid_trials.insert(summary.trial);
            }
            if let Some(parent) = summary.infrastructure_replacement_for {
                replacement_trials.insert(summary.trial);
                linked_failures.insert(parent);
            }
            let latest = latest_by_trial.entry(summary.trial).or_insert(summary);
            if (summary.memory_attempt, &summary.comparison_path)
                > (latest.memory_attempt, &latest.comparison_path)
            {
                *latest = summary;
            }
        }
        replacement.next_trial = max_trial.saturating_add(1);
        replacement.remaining =
            max_infrastructure_replacements.saturating_sub(replacement_trials.len());

        let queued = pending
            .iter()
            .filter(|scheduled| {
                scheduled.task_index == task_index && scheduled.profile_index == profile_index
            })
            .count();
        replacement.target_valid = requested_trials;
        replacement.valid = valid_trials.len().min(requested_trials);
        replacement.outstanding = queued;
        for failed_trial in latest_by_trial
            .values()
            .filter(|summary| {
                (summary.infrastructure_failure || summary.operational_error)
                    && !summary.oom_detected
                    && !linked_failures.contains(&summary.trial)
            })
            .map(|summary| summary.trial)
            .collect::<Vec<_>>()
        {
            let Some(scheduled) = replacement.next(task_index, profile_index, failed_trial) else {
                break;
            };
            pending.push_back(scheduled);
        }
    }
    skipped
}

impl DifferentialEvaluator {
    /// Starts a reusable matched differential-evaluation recipe.
    #[must_use]
    pub fn builder(nanocodex: NanocodexBuilder) -> DifferentialEvaluatorBuilder {
        DifferentialEvaluatorBuilder {
            nanocodex,
            codex: None,
            vm: None,
            output: PathBuf::from(DEFAULT_OUTPUT_DIRECTORY),
            thinking: Thinking::Medium,
            web_search: false,
            nanocodex_tool_mode: NanocodexToolMode::CodeModeOnly,
            codex_tool_mode: CodexToolMode::CodeModeOnly,
            nanocodex_build: None,
            max_concurrency: 1,
            max_memory_mb: None,
            max_infrastructure_replacements: 0,
            initial_guest_memory_mb: DEFAULT_DIFFERENTIAL_GUEST_MEMORY_MB,
            memory_profile_path: None,
        }
    }

    /// Runs one independent matched pair.
    ///
    /// # Errors
    ///
    /// Returns an error when the comparison cannot be prepared or retained.
    pub async fn task(&self, task: Task) -> DifferentialResult<DifferentialReport> {
        self.run_task(
            task,
            1,
            DifferentialProfile::new(
                self.inner.thinking,
                self.inner.nanocodex_tool_mode,
                self.inner.codex_tool_mode,
            ),
            None,
        )
        .await
    }

    async fn run_task(
        &self,
        task: Task,
        trial: usize,
        profile: DifferentialProfile,
        infrastructure_replacement_for: Option<usize>,
    ) -> DifferentialResult<DifferentialReport> {
        let scheduled = ScheduledComparison {
            task_index: 0,
            profile_index: 0,
            task,
            trial,
            profile,
            infrastructure_replacement_for,
            memory_attempt: 1,
            minimum_guest_memory_mb: None,
            memory_retry_for: None,
            queued_at: Utc::now(),
        };
        let memory_plan = self.memory_plan(&scheduled.task, None);
        let requested_memory_mb = memory_plan.pair_admission_memory_mb();
        let admission = self
            .inner
            .admission
            .acquire_many(DIFFERENTIAL_ARMS_PER_PAIR, requested_memory_mb)
            .await
            .ok_or_else(|| {
                DifferentialError::new(diff_error!("differential evaluator is draining"))
            })?;
        self.run_admitted_task(scheduled, memory_plan, admission)
            .await
    }

    async fn run_admitted_task(
        &self,
        scheduled: ScheduledComparison,
        memory_plan: DifferentialMemoryPlan,
        admission: AdmissionPermit,
    ) -> DifferentialResult<DifferentialReport> {
        let ScheduledComparison {
            task,
            trial,
            profile,
            infrastructure_replacement_for,
            memory_attempt,
            memory_retry_for,
            queued_at,
            ..
        } = scheduled;
        let admitted_at = Utc::now();
        let declared_memory_mb = differential_pair_memory_mb(task.resources().memory_mb);
        let requested_memory_mb = memory_plan.pair_admission_memory_mb();
        let admitted_memory_mb = self
            .inner
            .max_memory_mb
            .map_or(requested_memory_mb, |limit| requested_memory_mb.min(limit));
        let inner = &self.inner;
        let result = DifferentialComparison {
            task,
            trial,
            nanocodex: inner.nanocodex.clone(),
            codex_sha256: inner.codex_sha256.clone(),
            codex_release: Arc::clone(&inner.codex_release),
            codex_auth: inner.codex_auth.clone(),
            vm: Arc::clone(&inner.vm),
            output: inner.output.clone(),
            thinking: profile.thinking,
            web_search: inner.web_search,
            nanocodex_tool_mode: profile.nanocodex_tool_mode,
            codex_tool_mode: profile.codex_tool_mode,
            nanocodex_build: inner.nanocodex_build.clone(),
            schedule: DifferentialSchedule {
                queued_at,
                admitted_at,
                queue_duration_ms: admitted_at
                    .signed_duration_since(queued_at)
                    .num_milliseconds()
                    .max(0)
                    .try_into()
                    .unwrap_or(u64::MAX),
                declared_pair_memory_mb: declared_memory_mb,
                requested_pair_memory_mb: requested_memory_mb,
                admitted_pair_memory_mb: admitted_memory_mb,
                configured_guest_memory_mb: memory_plan.guest_memory_mb,
                nanocodex_admission_memory_mb: memory_plan.nanocodex_admission_memory_mb,
                codex_admission_memory_mb: memory_plan.codex_admission_memory_mb,
                memory_attempt,
                memory_retry_for,
                max_concurrency: inner.max_concurrency,
                max_memory_mb: inner.max_memory_mb,
                max_infrastructure_replacements: inner.max_infrastructure_replacements,
                infrastructure_replacement_for,
            },
            memory_plan,
            admission,
        }
        .run()
        .await;
        if let Ok(report) = &result {
            let mut memory = self
                .inner
                .memory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = memory.observe(report) {
                warn!(
                    task = report.task_name(),
                    error = %error,
                    "failed to persist differential memory observation"
                );
            }
        }
        result
    }

    fn memory_plan(
        &self,
        task: &Task,
        minimum_guest_memory_mb: Option<u64>,
    ) -> DifferentialMemoryPlan {
        self.inner
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .plan(task, minimum_guest_memory_mb)
    }

    /// Runs `count` independent matched pairs for one task.
    ///
    /// Configured invalid-pair replacements are retained after the requested
    /// trial coordinates, so the returned collection can contain more than
    /// `count` reports.
    ///
    /// Results preserve trial order even when pairs complete out of order.
    ///
    /// # Errors
    ///
    /// Returns an error after all admitted pairs finish when any comparison
    /// cannot be prepared or retained.
    pub async fn task_n(
        &self,
        task: Task,
        count: NonZeroUsize,
    ) -> DifferentialResult<DifferentialSweepResults> {
        self.run_tasks(
            vec![task],
            count,
            vec![DifferentialProfile::new(
                self.inner.thinking,
                self.inner.nanocodex_tool_mode,
                self.inner.codex_tool_mode,
            )],
        )
        .await
    }

    /// Runs one independent matched pair for every task.
    ///
    /// Configured invalid-pair replacements can add retained reports.
    ///
    /// Results preserve input order even when pairs complete out of order.
    ///
    /// # Errors
    ///
    /// Returns an error after all admitted pairs finish when any comparison
    /// cannot be prepared or retained.
    pub async fn tasks(&self, tasks: Vec<Task>) -> DifferentialResult<DifferentialSweepResults> {
        self.run_tasks(
            tasks,
            NonZeroUsize::MIN,
            vec![DifferentialProfile::new(
                self.inner.thinking,
                self.inner.nanocodex_tool_mode,
                self.inner.codex_tool_mode,
            )],
        )
        .await
    }

    async fn run_tasks(
        &self,
        tasks: Vec<Task>,
        count: NonZeroUsize,
        profiles: Vec<DifferentialProfile>,
    ) -> DifferentialResult<DifferentialSweepResults> {
        if tasks.is_empty() {
            return Err(DifferentialError::new(diff_error!(
                "differential matrix requires at least one task"
            )));
        }
        validate_differential_profiles(&profiles)?;
        let count = count.get();
        let (_guard, mut summaries) =
            prepare_differential_sweep(&self.inner, &tasks, &profiles, count)
                .map_err(DifferentialError::new)?;
        let profile_count = profiles.len();
        let (mut replacements, mut pending) = initial_differential_schedule(
            tasks,
            count,
            &profiles,
            self.inner.max_infrastructure_replacements,
        );
        let skipped = resume_differential_schedule(
            &mut pending,
            &mut replacements,
            &summaries,
            count,
            self.inner.max_infrastructure_replacements,
            profile_count,
        );
        let mut waiting_by_task = BTreeMap::<usize, VecDeque<ScheduledComparison>>::new();
        while let Some(scheduled) = pending.pop_front() {
            waiting_by_task
                .entry(scheduled.task_index)
                .or_default()
                .push_back(scheduled);
        }
        let mut preparations = FuturesUnordered::new();
        for (task_index, scheduled) in &waiting_by_task {
            let task_index = *task_index;
            let task = scheduled
                .front()
                .map(|scheduled| scheduled.task.clone())
                .ok_or_else(|| {
                    DifferentialError::new(diff_error!(
                        "differential scheduler created an empty task preparation queue"
                    ))
                })?;
            let vm = Arc::clone(&self.inner.vm);
            preparations.push(async move {
                let result = vm.environment(&task).await;
                (task_index, task, result)
            });
        }
        let mut in_flight = FuturesUnordered::new();
        let mut active_tasks = BTreeSet::new();
        let mut results = Vec::new();
        let mut preparation_errors = Vec::new();
        let mut draining = false;
        while !pending.is_empty() || !in_flight.is_empty() || !preparations.is_empty() {
            let capacity_generation = self.inner.admission.capacity_generation();
            if !draining && self.inner.admission.is_draining() {
                draining = true;
                pending.clear();
                waiting_by_task.clear();
                preparations.clear();
            }
            let mut pending_index = 0;
            while pending_index < pending.len() {
                if active_tasks.len() >= self.inner.max_concurrency {
                    break;
                }
                let Some(queued) = pending.get(pending_index) else {
                    return Err(DifferentialError::new(diff_error!(
                        "differential scheduler lost a queued coordinate"
                    )));
                };
                if !task_lane_available(&active_tasks, queued, self.inner.max_concurrency) {
                    pending_index += 1;
                    continue;
                }
                let memory_plan = self.memory_plan(&queued.task, queued.minimum_guest_memory_mb);
                let requested_memory_mb = memory_plan.pair_admission_memory_mb();
                match self
                    .inner
                    .admission
                    .try_acquire_many(DIFFERENTIAL_ARMS_PER_PAIR, requested_memory_mb)
                {
                    AdmissionAttempt::Acquired(admission) => {
                        let Some(scheduled) = pending.remove(pending_index) else {
                            return Err(DifferentialError::new(diff_error!(
                                "differential scheduler lost a ready coordinate"
                            )));
                        };
                        if !active_tasks.insert(scheduled.task_index) {
                            return Err(DifferentialError::new(diff_error!(
                                "differential scheduler admitted two comparisons for task {}",
                                scheduled.task.name()
                            )));
                        }
                        in_flight.push(run_scheduled_comparison(
                            self.clone(),
                            scheduled,
                            memory_plan,
                            admission,
                        ));
                    }
                    AdmissionAttempt::Unavailable => pending_index += 1,
                    AdmissionAttempt::Draining => {
                        draining = true;
                        break;
                    }
                }
            }

            if draining {
                pending.clear();
                waiting_by_task.clear();
                preparations.clear();
            }

            if in_flight.is_empty() && preparations.is_empty() {
                if draining || self.inner.admission.is_draining() {
                    break;
                }
                if pending.is_empty() {
                    break;
                }
            }

            enum SchedulerEvent<T> {
                Prepared(T),
                Completed((ScheduledComparison, DifferentialResult<DifferentialReport>)),
                Capacity,
            }
            let event = tokio::select! {
                prepared = preparations.next(), if !preparations.is_empty() => {
                    prepared.map(SchedulerEvent::Prepared)
                }
                completed = in_flight.next(), if !in_flight.is_empty() => {
                    completed.map(SchedulerEvent::Completed)
                }
                () = self.inner.admission.wait_for_change(capacity_generation), if (!pending.is_empty() || !preparations.is_empty()) && !draining => {
                    Some(SchedulerEvent::Capacity)
                }
                else => None,
            };
            let Some(event) = event else {
                break;
            };
            let SchedulerEvent::Completed((scheduled, result)) = event else {
                match event {
                    SchedulerEvent::Prepared((task_index, task, Ok(_environment))) => {
                        if let Some(mut ready) = waiting_by_task.remove(&task_index) {
                            info!(
                                task = task.name(),
                                ready_coordinates = ready.len(),
                                "differential task image is ready"
                            );
                            pending.append(&mut ready);
                        }
                    }
                    SchedulerEvent::Prepared((task_index, task, Err(error))) => {
                        waiting_by_task.remove(&task_index);
                        warn!(
                            task = task.name(),
                            error = %error,
                            "differential task preparation failed; other tasks remain runnable"
                        );
                        preparation_errors.push(DifferentialError::new(Box::new(error)));
                    }
                    SchedulerEvent::Capacity => {}
                    SchedulerEvent::Completed(_) => unreachable!("completed event was matched"),
                }
                continue;
            };
            if !active_tasks.remove(&scheduled.task_index) {
                return Err(DifferentialError::new(diff_error!(
                    "differential scheduler completed an inactive task lane for {}",
                    scheduled.task.name()
                )));
            }
            let task_index = scheduled.task_index;
            let profile_index = scheduled.profile_index;
            let trial = scheduled.trial;
            let memory_attempt = scheduled.memory_attempt;
            let memory_retry = result
                .as_ref()
                .ok()
                .and_then(|report| scheduled.memory_retry(report));
            if let Some(memory_retry) = memory_retry {
                info!(
                    task = memory_retry.task.name(),
                    trial,
                    memory_attempt = memory_retry.memory_attempt,
                    guest_memory_mb = memory_retry.minimum_guest_memory_mb,
                    "confirmed OOM retained; scheduled both arms again with more guest memory"
                );
                pending.push_front(memory_retry);
            } else {
                let replacement_index = task_index
                    .checked_mul(profile_count)
                    .and_then(|index| index.checked_add(profile_index));
                if let Some(state) = replacement_index.and_then(|index| replacements.get_mut(index))
                {
                    let valid = result.as_ref().is_ok_and(|report| {
                        !report.has_infrastructure_failure() && !report.has_operational_error()
                    });
                    state.complete(valid);
                }

                if let Ok(report) = &result
                    && report.oom_detected()
                {
                    warn!(
                        task = report.task_name(),
                        trial,
                        guest_memory_mb = report.configured_guest_memory_mb(),
                        "confirmed OOM persisted at the task-declared memory ceiling"
                    );
                } else if let Ok(report) = &result
                    && (report.has_infrastructure_failure() || report.has_operational_error())
                    && let Some(replacement_index) = replacement_index
                    && let Some(replacement) = replacements
                        .get_mut(replacement_index)
                        .and_then(|replacement| replacement.next(task_index, profile_index, trial))
                {
                    info!(
                        task = report.task_name(),
                        failed_trial = trial,
                        replacement_trial = replacement.trial,
                        remaining_replacements = replacements
                            .get(replacement_index)
                            .map_or(0, |state| state.remaining),
                        "scheduled a fresh pair to replace retained invalid comparison"
                    );
                    pending.push_front(replacement);
                }
            }
            results.push((task_index, profile_index, trial, memory_attempt, result));
        }
        if draining || self.inner.admission.is_draining() {
            return Err(DifferentialError::new(diff_error!(
                "differential evaluator is draining"
            )));
        }
        if let Some(error) = preparation_errors.into_iter().next() {
            return Err(error);
        }
        results.sort_unstable_by_key(|(task_index, profile_index, trial, memory_attempt, _)| {
            (*task_index, *profile_index, *trial, *memory_attempt)
        });
        let reports = results
            .into_iter()
            .map(|(_, _, _, _, result)| result)
            .collect::<DifferentialResult<Vec<_>>>()?;
        summaries.extend(reports.iter().map(DifferentialReportSummary::from_report));
        summaries.sort_unstable_by(|left, right| {
            (
                &left.task_root,
                &left.thinking,
                left.nanocodex_tool_mode.as_str(),
                left.codex_tool_mode.as_str(),
                left.trial,
                left.memory_attempt,
                &left.comparison_path,
            )
                .cmp(&(
                    &right.task_root,
                    &right.thinking,
                    right.nanocodex_tool_mode.as_str(),
                    right.codex_tool_mode.as_str(),
                    right.trial,
                    right.memory_attempt,
                    &right.comparison_path,
                ))
        });
        Ok(DifferentialSweepResults {
            reports,
            summaries,
            skipped,
        })
    }

    /// Runs `count` independent matched pairs for every task.
    ///
    /// Configured invalid-pair replacements are retained after each task's
    /// requested trial coordinates, so the returned collection can contain
    /// more than `tasks.len() * count` reports.
    ///
    /// Results are grouped in input task order and then trial order.
    ///
    /// # Errors
    ///
    /// Returns an error after all admitted pairs finish when any comparison
    /// cannot be prepared or retained.
    pub async fn tasks_n(
        &self,
        tasks: Vec<Task>,
        count: NonZeroUsize,
    ) -> DifferentialResult<DifferentialSweepResults> {
        self.run_tasks(
            tasks,
            count,
            vec![DifferentialProfile::new(
                self.inner.thinking,
                self.inner.nanocodex_tool_mode,
                self.inner.codex_tool_mode,
            )],
        )
        .await
    }

    /// Runs one centrally scheduled task × profile × trial matrix.
    ///
    /// Profiles are semantic identities: both arms receive the profile's
    /// reasoning effort and each arm receives its selected tool exposure.
    /// Images, staged executables, admission limits, and completion handling
    /// are shared by the complete matrix.
    ///
    /// # Errors
    ///
    /// Returns an error after admitted work drains when the profile list is
    /// empty, contains duplicates, or a comparison cannot be retained.
    pub async fn tasks_n_with_profiles(
        &self,
        tasks: Vec<Task>,
        count: NonZeroUsize,
        profiles: Vec<DifferentialProfile>,
    ) -> DifferentialResult<DifferentialSweepResults> {
        self.run_tasks(tasks, count, profiles).await
    }

    /// Returns the maximum active-arm capacity expressed in pair equivalents.
    #[must_use]
    pub fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }

    /// Returns the optional target ceiling on measured host memory across live arms.
    #[must_use]
    pub fn max_memory_mb(&self) -> Option<u64> {
        self.inner.max_memory_mb
    }

    /// Returns the per-task budget for replacing invalid pairs.
    #[must_use]
    pub fn max_infrastructure_replacements(&self) -> usize {
        self.inner.max_infrastructure_replacements
    }

    /// Stops admitting pairs that have not started.
    ///
    /// Admitted work continues to completion. The return value is the total
    /// number of pairs admitted since this evaluator was built.
    pub fn begin_drain(&self) -> usize {
        self.inner.admission.begin_drain()
    }
}

fn initial_differential_schedule(
    tasks: Vec<Task>,
    count: usize,
    profiles: &[DifferentialProfile],
    max_infrastructure_replacements: usize,
) -> (
    Vec<InfrastructureReplacementState>,
    VecDeque<ScheduledComparison>,
) {
    let mut replacements = Vec::new();
    let mut pending = VecDeque::new();
    for (task_index, task) in tasks.into_iter().enumerate() {
        for (profile_index, profile) in profiles.iter().copied().enumerate() {
            replacements.push(InfrastructureReplacementState {
                task: task.clone(),
                profile,
                next_trial: count.saturating_add(1),
                remaining: max_infrastructure_replacements,
                target_valid: count,
                valid: 0,
                outstanding: count,
            });
            pending.extend((1..=count).map(|trial| ScheduledComparison {
                task_index,
                profile_index,
                task: task.clone(),
                trial,
                profile,
                infrastructure_replacement_for: None,
                memory_attempt: 1,
                minimum_guest_memory_mb: None,
                memory_retry_for: None,
                queued_at: Utc::now(),
            }));
        }
    }
    (replacements, pending)
}

impl InfrastructureReplacementState {
    fn complete(&mut self, valid: bool) {
        self.outstanding = self.outstanding.saturating_sub(1);
        if valid {
            self.valid = self.valid.saturating_add(1).min(self.target_valid);
        }
    }

    fn next(
        &mut self,
        task_index: usize,
        profile_index: usize,
        infrastructure_replacement_for: usize,
    ) -> Option<ScheduledComparison> {
        if self.remaining == 0 || self.valid.saturating_add(self.outstanding) >= self.target_valid {
            return None;
        }
        let trial = self.next_trial;
        self.remaining -= 1;
        self.outstanding = self.outstanding.saturating_add(1);
        if let Some(next_trial) = trial.checked_add(1) {
            self.next_trial = next_trial;
        } else {
            self.remaining = 0;
        }
        Some(ScheduledComparison {
            task_index,
            profile_index,
            task: self.task.clone(),
            trial,
            profile: self.profile,
            infrastructure_replacement_for: Some(infrastructure_replacement_for),
            memory_attempt: 1,
            minimum_guest_memory_mb: None,
            memory_retry_for: None,
            queued_at: Utc::now(),
        })
    }
}

impl ScheduledComparison {
    fn memory_retry(&self, report: &DifferentialReport) -> Option<Self> {
        let next_guest_memory_mb = report.next_guest_memory_mb()?;
        Some(Self {
            task_index: self.task_index,
            profile_index: self.profile_index,
            task: self.task.clone(),
            trial: self.trial,
            profile: self.profile,
            infrastructure_replacement_for: self.infrastructure_replacement_for,
            memory_attempt: self.memory_attempt.saturating_add(1),
            minimum_guest_memory_mb: Some(next_guest_memory_mb),
            memory_retry_for: Some(report.comparison_path().to_path_buf()),
            queued_at: Utc::now(),
        })
    }
}

fn validate_differential_profiles(profiles: &[DifferentialProfile]) -> DifferentialResult<()> {
    if profiles.is_empty() {
        return Err(DifferentialError::new(diff_error!(
            "differential matrix requires at least one profile"
        )));
    }
    for (index, profile) in profiles.iter().enumerate() {
        if profiles[..index].contains(profile) {
            return Err(DifferentialError::new(diff_error!(
                "differential matrix contains duplicate profile {}",
                profile.name()
            )));
        }
    }
    Ok(())
}

async fn run_scheduled_comparison(
    evaluator: DifferentialEvaluator,
    scheduled: ScheduledComparison,
    memory_plan: DifferentialMemoryPlan,
    admission: AdmissionPermit,
) -> (ScheduledComparison, DifferentialResult<DifferentialReport>) {
    let result = evaluator
        .run_admitted_task(scheduled.clone(), memory_plan, admission)
        .await;
    (scheduled, result)
}

fn task_lane_available(
    active_tasks: &BTreeSet<usize>,
    scheduled: &ScheduledComparison,
    max_active_tasks: usize,
) -> bool {
    active_tasks.len() < max_active_tasks && !active_tasks.contains(&scheduled.task_index)
}

impl DifferentialComparison {
    /// Runs both agents concurrently and retains one complete comparison.
    ///
    /// An incomplete arm remains a successful, inspectable report. This method
    /// returns an error only when the comparison itself cannot be prepared or
    /// retained.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid executable inputs, VM preparation failure,
    /// artifact I/O failure, or evaluator setup that prevents a report.
    async fn run(self) -> DifferentialResult<DifferentialReport> {
        self.run_inner().await.map_err(DifferentialError::new)
    }

    async fn run_inner(self) -> InternalResult<DifferentialReport> {
        let Self {
            task,
            trial,
            nanocodex,
            codex_sha256,
            codex_release,
            codex_auth,
            vm,
            output,
            thinking,
            web_search,
            nanocodex_tool_mode,
            codex_tool_mode,
            nanocodex_build,
            schedule,
            memory_plan,
            admission,
        } = self;
        let codex_path = codex_release.root.join("codex");
        let started_at = Utc::now();
        let started = Instant::now();
        let comparison_id = Uuid::now_v7();
        let comparison_directory = output.join(differential_comparison_name(
            &task,
            DifferentialProfile::new(thinking, nanocodex_tool_mode, codex_tool_mode),
            trial,
            comparison_id,
        ));
        fs::create_dir(&comparison_directory).wrap_err_with(|| {
            format!(
                "failed to create comparison directory {}",
                comparison_directory.display()
            )
        })?;
        let progress_path = comparison_directory.join(PROGRESS_FILE);
        let (progress, progress_recorder) =
            DiffProgress::start(progress_path.clone(), started).await?;
        progress.emit(
            "runner",
            "comparison.started",
            format!(
                "{} · {MODEL} / {thinking} · nanocodex {} · stock {}",
                task.name(),
                nanocodex_tool_mode.as_str(),
                codex_tool_mode.as_str()
            ),
        );

        let guest_codex_version = Arc::new(OnceLock::new());
        let vm_resources = Arc::new(
            prepare_diff_vm_resources(
                &task,
                &vm,
                memory_plan.guest_memory_mb,
                web_search,
                &codex_release,
            )
            .await?,
        );
        let codex = CodexExec::new(&codex_path, MODEL, thinking.as_str())?
            .web_search(web_search)
            .tool_mode(codex_tool_mode);

        let nanocodex = nanocodex.thinking(thinking);
        let nanocodex_memory = Arc::new(OnceLock::<VmAttemptMemory>::new());
        let nanocodex_memory_slot = Arc::clone(&nanocodex_memory);
        let nanocodex_evaluator = Evaluator::new_builder(nanocodex.clone())
            .output_directory(comparison_directory.join("nanocodex"))
            .vm_with(
                vm_resources.nanocodex_backend(),
                move |_attempt, builder, runtime| {
                    let _ = nanocodex_memory_slot.set(runtime.memory_observation());
                    runtime.nanocodex_with_exposure(builder, nanocodex_tool_mode.exposure())
                },
            );
        let codex_backend = vm_resources.codex_backend();
        let codex_resources = Arc::clone(&vm_resources);
        let codex_config = codex.clone();
        let codex_auth = codex_auth.clone();
        let version = Arc::clone(&guest_codex_version);
        let codex_progress = progress.clone();
        let codex_memory = Arc::new(OnceLock::<VmAttemptMemory>::new());
        let codex_memory_slot = Arc::clone(&codex_memory);
        let codex_evaluator = Evaluator::new_builder(nanocodex)
            .output_directory(comparison_directory.join("codex"))
            .vm_with(codex_backend, move |attempt, _builder, runtime| {
                let _ = codex_memory_slot.set(runtime.memory_observation());
                codex_resources.codex_attempt(
                    runtime,
                    attempt,
                    codex_config.clone(),
                    codex_auth.clone(),
                    Arc::clone(&version),
                    codex_progress.clone(),
                )
            });
        let projection = TrajectoryProjection::Codex {
            version: CodexVersion::Guest(Arc::clone(&guest_codex_version)),
        };
        let nanocodex_release_memory_mb = releasable_differential_arm_memory_mb(
            memory_plan.nanocodex_admission_memory_mb,
            memory_plan.pair_admission_memory_mb(),
            schedule.max_memory_mb,
        );
        let codex_release_memory_mb = releasable_differential_arm_memory_mb(
            memory_plan.codex_admission_memory_mb,
            memory_plan.pair_admission_memory_mb(),
            schedule.max_memory_mb,
        );
        let (mut nanocodex_arm, mut codex_arm) = join_differential_arms(
            admission,
            nanocodex_release_memory_mb,
            codex_release_memory_mb,
            run_arm(
                task.clone(),
                nanocodex_evaluator,
                TrajectoryProjection::Nanocodex,
                true,
                progress.clone(),
            ),
            run_arm(
                task.clone(),
                codex_evaluator,
                projection,
                true,
                progress.clone(),
            ),
        )
        .await;
        nanocodex_arm.memory = nanocodex_memory
            .get()
            .map(|memory| ArmMemoryReport::from(memory.snapshot()));
        codex_arm.memory = codex_memory
            .get()
            .map(|memory| ArmMemoryReport::from(memory.snapshot()));
        let codex_version = guest_codex_version
            .get()
            .cloned()
            .unwrap_or_else(|| "unavailable".to_owned());

        let oom_detected = [&nanocodex_arm, &codex_arm].into_iter().any(|arm| {
            arm.memory
                .as_ref()
                .is_some_and(|memory| memory.oom_detected)
        });
        let classification = if oom_detected {
            DifferentialClassification::Incomplete
        } else {
            DifferentialClassification::from_arms(&nanocodex_arm, &codex_arm)
        };
        let trajectory_comparison = TrajectoryComparison::from_arms(&nanocodex_arm, &codex_arm);
        let api_comparison_path = comparison_directory.join(API_COMPARISON_FILE);
        let (api_comparison, retained_api_comparison, api_comparison_error) =
            match retain_api_comparison(&api_comparison_path, &nanocodex_arm, &codex_arm) {
                Ok(summary) => (summary, Some(api_comparison_path), None),
                Err(error) => (
                    ApiComparisonSummary::unavailable(),
                    None,
                    Some(format!("{error:#}")),
                ),
            };
        nanocodex_arm
            .summary
            .apply_model_visible_tool_calls(api_comparison.event_loop.nanocodex.as_ref());
        codex_arm
            .summary
            .apply_model_visible_tool_calls(api_comparison.event_loop.codex.as_ref());
        let profile_validation_error = validate_differential_profile(
            &api_comparison,
            MODEL,
            thinking.as_str(),
            nanocodex_tool_mode,
            codex_tool_mode,
            web_search,
        );
        progress.emit("runner", "comparison.completed", classification.as_str());
        let progress_error = progress_recorder
            .finish(progress)
            .await
            .err()
            .map(|error| format!("{error:#}"));
        let comparison_path = comparison_directory.join(COMPARISON_FILE);
        let report = DifferentialReport {
            schema_version: COMPARISON_SCHEMA_VERSION,
            id: comparison_id,
            task: TaskIdentity {
                name: task.name().to_owned(),
                root: task.root().to_path_buf(),
                content_digest: task.content_digest().to_owned(),
            },
            trial,
            model: MODEL.to_owned(),
            thinking: thinking.to_string(),
            policy: ComparisonPolicy {
                runner: "nanocodex_eval",
                environment: "micro_vm",
                attempts_per_agent: 1,
                execution_mode: "concurrent",
                web_search,
                codex_ephemeral: true,
                codex_approval_policy: "never",
                codex_sandbox: "danger_full_access",
                nanocodex_tool_mode,
                codex_tool_mode,
                multi_agent: "disabled",
                reasoning_summary: "auto",
                expected_nanocodex_visible_tools: expected_nanocodex_visible_tools(
                    nanocodex_tool_mode,
                    web_search,
                ),
            },
            started_at,
            finished_at: Utc::now(),
            duration_ms: elapsed_ms(started),
            schedule,
            classification,
            trajectory_comparison,
            api_comparison,
            nanocodex_build,
            codex_build: ExecutableIdentity {
                path: codex_release.root.join("codex"),
                version: codex_version,
                git_sha: None,
                built_at: None,
                sha256: codex_sha256,
            },
            nanocodex: nanocodex_arm,
            codex: codex_arm,
            artifacts: ComparisonArtifacts {
                directory: comparison_directory,
                comparison: comparison_path.clone(),
                progress: progress_path,
                progress_error,
                api_comparison: retained_api_comparison,
                api_comparison_error,
                profile_validation_error,
            },
        };
        write_json_atomic(&comparison_path, &report)?;
        Ok(report)
    }
}

impl DifferentialEvaluatorBuilder {
    /// Selects the pinned stock-Codex executable and its guest auth.
    #[must_use]
    pub fn codex(mut self, executable: impl Into<PathBuf>, auth: CodexAuth) -> Self {
        self.codex = Some((executable.into(), auth));
        self
    }

    /// Selects the prepared, matched VM resources used by both arms.
    #[must_use]
    pub fn vm(mut self, vm: VmResources) -> Self {
        self.vm = Some(Arc::new(vm));
        self
    }

    /// Selects the parent directory for retained comparisons.
    #[must_use]
    pub fn output_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.output = directory.into();
        self
    }

    /// Pins the shared reasoning effort used by both agents.
    #[must_use]
    pub const fn thinking(mut self, thinking: Thinking) -> Self {
        self.thinking = thinking;
        self
    }

    /// Selects whether both agents expose standalone web search.
    #[must_use]
    pub const fn web_search(mut self, enabled: bool) -> Self {
        self.web_search = enabled;
        self
    }

    /// Selects Nanocodex's model-visible tool exposure.
    #[must_use]
    pub const fn nanocodex_tool_mode(mut self, tool_mode: NanocodexToolMode) -> Self {
        self.nanocodex_tool_mode = tool_mode;
        self
    }

    /// Selects stock Codex's model-visible tool exposure.
    #[must_use]
    pub const fn codex_tool_mode(mut self, tool_mode: CodexToolMode) -> Self {
        self.codex_tool_mode = tool_mode;
        self
    }

    /// Records the embedding Nanocodex executable used as the VMM entrypoint.
    #[must_use]
    pub fn nanocodex_executable(mut self, identity: ExecutableIdentity) -> Self {
        self.nanocodex_build = Some(identity);
        self
    }

    /// Sets the active task-lane limit and arm capacity in matched-pair equivalents.
    ///
    /// At most this many distinct tasks may have a comparison in flight. A pair
    /// initially occupies two arm slots; each completed arm returns one slot and
    /// its measured-memory charge. The default is one task lane and one pair
    /// equivalent. [`Self::prepare`] rejects zero.
    #[must_use]
    pub const fn max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    /// Bounds the sum of learned host-RSS estimates across live arms. Both arms
    /// are charged when a pair starts; each charge and active-arm slot is
    /// released after that arm's evaluator and VM cleanup finish. A pair that
    /// exceeds the target runs alone.
    #[must_use]
    pub const fn max_memory_mb(mut self, max_memory_mb: u64) -> Self {
        self.max_memory_mb = Some(max_memory_mb);
        self
    }

    /// Sets the low per-arm guest allocation used until a task has measured
    /// memory history. The allocation is always capped by the task declaration.
    #[must_use]
    pub const fn initial_guest_memory_mb(mut self, memory_mb: u64) -> Self {
        self.initial_guest_memory_mb = memory_mb;
        self
    }

    /// Selects the durable task-memory profile shared by future sweeps.
    #[must_use]
    pub fn memory_profile_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.memory_profile_path = Some(path.into());
        self
    }

    /// Replaces retained pairs that are invalid for comparison.
    ///
    /// Infrastructure failures and operationally invalid comparisons consume
    /// the same bounded per-task budget. Replacement pairs use fresh trial
    /// coordinates after the requested trials and remain linked to the retained
    /// invalid evidence. The default is zero.
    #[must_use]
    pub const fn max_infrastructure_replacements(
        mut self,
        max_infrastructure_replacements: usize,
    ) -> Self {
        self.max_infrastructure_replacements = max_infrastructure_replacements;
        self
    }

    /// Validates required components and asynchronously prepares a reusable evaluator.
    ///
    /// # Errors
    ///
    /// Returns an error when Codex, VM resources, or executable identity is
    /// missing.
    pub async fn prepare(
        self,
    ) -> std::result::Result<DifferentialEvaluator, DifferentialBuildError> {
        let Some(max_active_arms) = self.max_concurrency.checked_mul(DIFFERENTIAL_ARMS_PER_PAIR)
        else {
            return Err(DifferentialBuildError::InvalidConcurrency);
        };
        if max_active_arms == 0 {
            return Err(DifferentialBuildError::InvalidConcurrency);
        }
        if self.max_memory_mb == Some(0) {
            return Err(DifferentialBuildError::InvalidMemory);
        }
        if self.initial_guest_memory_mb == 0 {
            return Err(DifferentialBuildError::InvalidInitialGuestMemory);
        }
        let vm = self.vm.ok_or(DifferentialBuildError::MissingVm)?;
        let (codex_binary, codex_auth) = self.codex.ok_or(DifferentialBuildError::MissingCodex)?;
        let nanocodex_identity = self
            .nanocodex_build
            .ok_or(DifferentialBuildError::MissingNanocodexIdentity)?;
        let output = self.output;
        let memory_profile_path = self.memory_profile_path;
        let initial_guest_memory_mb = self.initial_guest_memory_mb;
        let (codex_sha256, nanocodex_build, output, memory, codex_release) =
            tokio::task::spawn_blocking(move || {
                let (codex_binary, codex_sha256) = resolve_executable(&codex_binary, "stock Codex")
                    .map_err(|error| {
                        DifferentialBuildError::Executable(DifferentialError::new(error))
                    })?;
                let nanocodex_build = nanocodex_identity.resolve("Nanocodex").map_err(|error| {
                    DifferentialBuildError::Executable(DifferentialError::new(error))
                })?;
                let output = prepare_output_parent(&output).map_err(|error| {
                    DifferentialBuildError::Assets(DifferentialError::new(error))
                })?;
                let memory_profile_path = memory_profile_path
                    .unwrap_or_else(|| output.join("differential-memory-profiles.json"));
                let memory =
                    DifferentialMemoryPlanner::load(memory_profile_path, initial_guest_memory_mb)
                        .map_err(|error| {
                        DifferentialBuildError::MemoryProfiles(DifferentialError::new(error))
                    })?;
                let codex_release =
                    prepare_diff_codex_release(&output, &codex_binary).map_err(|error| {
                        DifferentialBuildError::Assets(DifferentialError::new(error))
                    })?;
                Ok::<_, DifferentialBuildError>((
                    codex_sha256,
                    nanocodex_build,
                    output,
                    memory,
                    codex_release,
                ))
            })
            .await??;
        Ok(DifferentialEvaluator {
            inner: Arc::new(DifferentialEvaluatorInner {
                nanocodex: self.nanocodex,
                codex_sha256,
                codex_release: Arc::new(codex_release),
                codex_auth,
                vm,
                output,
                thinking: self.thinking,
                web_search: self.web_search,
                nanocodex_tool_mode: self.nanocodex_tool_mode,
                codex_tool_mode: self.codex_tool_mode,
                nanocodex_build,
                admission: Arc::new(AdmissionController::new(
                    max_active_arms,
                    self.max_memory_mb,
                )),
                max_concurrency: self.max_concurrency,
                max_memory_mb: self.max_memory_mb,
                max_infrastructure_replacements: self.max_infrastructure_replacements,
                memory: Mutex::new(memory),
            }),
        })
    }
}

impl DifferentialSweepResults {
    /// Returns reports produced by this process after resume filtering.
    #[must_use]
    pub fn reports(&self) -> &[DifferentialReport] {
        &self.reports
    }

    /// Returns the complete durable index, including resumed reports.
    #[must_use]
    pub fn summaries(&self) -> &[DifferentialReportSummary] {
        &self.summaries
    }

    /// Returns the number of already-valid requested coordinates not rerun.
    #[must_use]
    pub const fn skipped(&self) -> usize {
        self.skipped
    }
}

impl DifferentialReportSummary {
    fn from_report(report: &DifferentialReport) -> Self {
        Self {
            task_name: report.task.name.clone(),
            task_root: report.task.root.clone(),
            task_content_digest: report.task.content_digest.clone(),
            trial: report.trial,
            thinking: report.thinking.clone(),
            nanocodex_tool_mode: report.policy.nanocodex_tool_mode,
            codex_tool_mode: report.policy.codex_tool_mode,
            classification: report.classification,
            infrastructure_failure: report.has_infrastructure_failure(),
            operational_error: report.has_operational_error(),
            oom_detected: report.oom_detected(),
            memory_attempt: report.memory_attempt(),
            configured_guest_memory_mb: report.configured_guest_memory_mb(),
            declared_guest_memory_mb: report.declared_arm_memory_mb(),
            infrastructure_replacement_for: report.schedule.infrastructure_replacement_for,
            comparison_path: report.artifacts.comparison.clone(),
        }
    }

    fn matches(&self, task: &Task, profile: DifferentialProfile) -> bool {
        self.task_root == task.root()
            && self.task_name == task.name()
            && self.task_content_digest == task.content_digest()
            && self.thinking == profile.thinking.as_str()
            && self.nanocodex_tool_mode == profile.nanocodex_tool_mode
            && self.codex_tool_mode == profile.codex_tool_mode
    }

    const fn is_valid(&self) -> bool {
        !self.infrastructure_failure && !self.operational_error
    }

    /// Returns the retained task name.
    #[must_use]
    pub fn task_name(&self) -> &str {
        &self.task_name
    }

    /// Returns the shared reasoning effort.
    #[must_use]
    pub fn thinking(&self) -> &str {
        &self.thinking
    }

    /// Returns Nanocodex's tool-exposure treatment.
    #[must_use]
    pub const fn nanocodex_tool_mode(&self) -> NanocodexToolMode {
        self.nanocodex_tool_mode
    }

    /// Returns stock Codex's tool-exposure treatment.
    #[must_use]
    pub const fn codex_tool_mode(&self) -> CodexToolMode {
        self.codex_tool_mode
    }

    /// Returns the one-indexed retained trial coordinate.
    #[must_use]
    pub const fn trial(&self) -> usize {
        self.trial
    }

    /// Returns the verifier relationship retained for the pair.
    #[must_use]
    pub const fn classification(&self) -> DifferentialClassification {
        self.classification
    }

    /// Returns whether this attempt was unscored infrastructure evidence.
    #[must_use]
    pub const fn has_infrastructure_failure(&self) -> bool {
        self.infrastructure_failure
    }

    /// Returns whether this attempt retained an operational error.
    #[must_use]
    pub const fn has_operational_error(&self) -> bool {
        self.operational_error
    }

    /// Returns whether this attempt retained confirmed OOM evidence.
    #[must_use]
    pub const fn oom_detected(&self) -> bool {
        self.oom_detected
    }

    /// Returns the per-arm guest allocation used for this attempt.
    #[must_use]
    pub const fn configured_guest_memory_mb(&self) -> u64 {
        self.configured_guest_memory_mb
    }

    /// Returns the one-indexed memory attempt for this logical trial.
    #[must_use]
    pub const fn memory_attempt(&self) -> usize {
        self.memory_attempt
    }

    /// Returns the durable comparison record path.
    #[must_use]
    pub fn comparison_path(&self) -> &Path {
        &self.comparison_path
    }
}

impl DifferentialReport {
    /// Returns the matched verifier classification.
    #[must_use]
    pub const fn classification(&self) -> DifferentialClassification {
        self.classification
    }

    /// Returns the retained task name.
    #[must_use]
    pub fn task_name(&self) -> &str {
        &self.task.name
    }

    /// Returns the Nanocodex tool treatment used by this coordinate.
    #[must_use]
    pub const fn nanocodex_tool_mode(&self) -> NanocodexToolMode {
        self.policy.nanocodex_tool_mode
    }

    /// Returns the stock-Codex tool treatment used by this coordinate.
    #[must_use]
    pub const fn codex_tool_mode(&self) -> CodexToolMode {
        self.policy.codex_tool_mode
    }

    /// Returns the reasoning effort shared by both arms.
    #[must_use]
    pub fn thinking(&self) -> &str {
        &self.thinking
    }

    /// Returns the one-indexed independent trial coordinate.
    #[must_use]
    pub const fn trial(&self) -> usize {
        self.trial
    }

    /// Returns the durable comparison record path.
    #[must_use]
    pub fn comparison_path(&self) -> &Path {
        &self.artifacts.comparison
    }

    /// Returns whether either arm or a derived comparison failed operationally.
    #[must_use]
    pub fn has_operational_error(&self) -> bool {
        self.artifacts.progress_error.is_some()
            || self.artifacts.api_comparison_error.is_some()
            || self.artifacts.profile_validation_error.is_some()
            || [&self.nanocodex, &self.codex].into_iter().any(|arm| {
                arm.operational_error.is_some()
                    || arm.event_error.is_some()
                    || arm.trajectory_error.is_some()
                    || arm.api_capture_error.is_some()
            })
    }

    /// Returns whether either retained arm ended in a semantic infrastructure
    /// failure and therefore has no trustworthy benchmark score.
    #[must_use]
    pub fn has_infrastructure_failure(&self) -> bool {
        self.oom_detected()
            || [&self.nanocodex, &self.codex].into_iter().any(|arm| {
                arm.outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.outcome() == EvalOutcome::InfrastructureError)
            })
    }

    /// Returns whether guest counters or kernel diagnostics confirmed an OOM.
    #[must_use]
    pub fn oom_detected(&self) -> bool {
        [&self.nanocodex, &self.codex].into_iter().any(|arm| {
            arm.memory
                .as_ref()
                .is_some_and(|memory| memory.oom_detected)
        })
    }

    /// Returns the per-arm guest allocation used by this memory attempt.
    #[must_use]
    pub const fn configured_guest_memory_mb(&self) -> u64 {
        self.schedule.configured_guest_memory_mb
    }

    /// Returns the one-indexed memory attempt for this logical trial.
    #[must_use]
    pub const fn memory_attempt(&self) -> usize {
        self.schedule.memory_attempt
    }

    const fn declared_arm_memory_mb(&self) -> u64 {
        self.schedule.declared_pair_memory_mb / 2
    }

    fn next_guest_memory_mb(&self) -> Option<u64> {
        if !self.oom_detected() {
            return None;
        }
        next_guest_memory_after_oom(
            self.configured_guest_memory_mb(),
            self.declared_arm_memory_mb(),
        )
    }

    fn is_memory_calibration_success(&self) -> bool {
        !self.oom_detected()
            && [&self.nanocodex, &self.codex].into_iter().all(|arm| {
                arm.outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.outcome() != EvalOutcome::InfrastructureError)
            })
    }

    /// Renders the stable plain-text summary used by command-line consumers.
    #[must_use]
    pub fn human_summary(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "{}", self.classification.as_str());
        let _ = writeln!(output, "task: {} · trial: {}", self.task.name, self.trial);
        let _ = writeln!(
            output,
            "profile: {} · nanocodex {} · stock {}",
            self.thinking,
            self.policy.nanocodex_tool_mode.as_str(),
            self.policy.codex_tool_mode.as_str(),
        );
        let _ = writeln!(
            output,
            "memory: attempt {} · {} MiB guest/arm · {}+{} MiB host admission",
            self.schedule.memory_attempt,
            self.schedule.configured_guest_memory_mb,
            self.schedule.nanocodex_admission_memory_mb,
            self.schedule.codex_admission_memory_mb,
        );
        append_arm_summary(&mut output, "nanocodex", &self.nanocodex);
        append_arm_summary(&mut output, "codex", &self.codex);
        append_model_visible_tool_summary(&mut output, &self.api_comparison.event_loop);
        append_first_generation_divergence(&mut output, &self.api_comparison.event_loop);
        append_unpaired_tail_summary(&mut output, &self.api_comparison.event_loop);
        let _ = writeln!(
            output,
            "live progress: {}",
            self.artifacts.progress.display()
        );
        if let Some(error) = &self.artifacts.progress_error {
            let _ = writeln!(output, "live progress error: {error}");
        }
        if let Some(path) = &self.artifacts.api_comparison {
            let _ = writeln!(output, "API comparison: {}", path.display());
        }
        if let Some(error) = &self.artifacts.api_comparison_error {
            let _ = writeln!(output, "API comparison error: {error}");
        }
        if let Some(error) = &self.artifacts.profile_validation_error {
            let _ = writeln!(output, "matched-profile error: {error}");
        }
        let _ = writeln!(
            output,
            "comparison: {}",
            self.artifacts.comparison.display()
        );
        output
    }
}

impl DifferentialReanalysis {
    /// Returns the rebuilt JSON report.
    #[must_use]
    pub const fn comparison(&self) -> &serde_json::Value {
        &self.comparison
    }

    /// Returns the durable comparison record that was updated.
    #[must_use]
    pub fn comparison_path(&self) -> &Path {
        &self.comparison_path
    }

    /// Returns the derived API comparison path when raw captures were available.
    #[must_use]
    pub fn api_comparison_path(&self) -> Option<&Path> {
        self.api_comparison_path.as_deref()
    }

    /// Returns a stable plain-text summary for command-line consumers.
    #[must_use]
    pub fn human_summary(&self) -> &str {
        &self.human_summary
    }
}

impl DifferentialClassification {
    fn from_arms(nanocodex: &ArmReport, codex: &ArmReport) -> Self {
        if nanocodex
            .memory
            .as_ref()
            .is_some_and(|memory| memory.oom_detected)
            || codex
                .memory
                .as_ref()
                .is_some_and(|memory| memory.oom_detected)
            || nanocodex.operational_error.is_some()
            || nanocodex.event_error.is_some()
            || nanocodex.trajectory_error.is_some()
            || nanocodex.api_capture_error.is_some()
            || nanocodex.summary.is_infrastructure_failure()
            || codex.operational_error.is_some()
            || codex.event_error.is_some()
            || codex.trajectory_error.is_some()
            || codex.api_capture_error.is_some()
            || codex.summary.is_infrastructure_failure()
        {
            return Self::Incomplete;
        }
        match (
            matches!(nanocodex.summary.status, ArmStatus::Passed),
            matches!(codex.summary.status, ArmStatus::Passed),
        ) {
            (true, true) => Self::BothPassed,
            (false, true) => Self::CodexOnlyPassed,
            (true, false) => Self::NanocodexOnlyPassed,
            (false, false) => Self::NeitherPassed,
        }
    }

    /// Returns the stable serialized spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BothPassed => "both_passed",
            Self::CodexOnlyPassed => "codex_only_passed",
            Self::NanocodexOnlyPassed => "nanocodex_only_passed",
            Self::NeitherPassed => "neither_passed",
            Self::Incomplete => "incomplete",
        }
    }
}

impl ArmStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::VerifierFailed => "verifier_failed",
            Self::Unscored => "unscored",
            Self::RunnerError => "runner_error",
        }
    }
}

impl TrajectorySummary {
    fn new(trajectory: &AtifTrajectory) -> Self {
        let mut agent_steps = 0_usize;
        let mut message_steps = 0_usize;
        let mut reasoning_steps = 0_usize;
        let mut tool_sequence = Vec::new();
        for step in &trajectory.steps {
            if matches!(step.source, AtifSource::Agent) {
                agent_steps = agent_steps.saturating_add(1);
            }
            if !step.message.is_empty() {
                message_steps = message_steps.saturating_add(1);
            }
            if step
                .reasoning_content
                .as_ref()
                .is_some_and(|reasoning| !reasoning.is_empty())
            {
                reasoning_steps = reasoning_steps.saturating_add(1);
            }
            if let Some(tool_calls) = &step.tool_calls {
                tool_sequence.extend(
                    tool_calls
                        .iter()
                        .map(|tool_call| tool_call.function_name.clone()),
                );
            }
        }
        Self {
            total_steps: count_u32(trajectory.steps.len()),
            agent_steps: count_u32(agent_steps),
            message_steps: count_u32(message_steps),
            reasoning_steps: count_u32(reasoning_steps),
            tool_calls: count_u32(trajectory.tool_call_count()),
            observations: count_u32(trajectory.observation_count()),
            model_calls: trajectory
                .steps
                .iter()
                .filter(|step| matches!(step.source, AtifSource::Agent))
                .try_fold(0_u32, |total, step| {
                    step.llm_call_count.map(|count| total.saturating_add(count))
                }),
            tool_projection: match trajectory.agent.name.as_str() {
                "nanocodex" => "lifecycle_outer_and_nested_tools",
                "codex" => "stock_cli_completed_items",
                _ => "atif_tool_calls",
            },
            tool_sequence,
            shell_polling: ShellPollingSummary::new(&trajectory.steps),
            usage_completeness: trajectory.final_metrics.extra.usage_completeness,
            runtime_completeness: trajectory.final_metrics.extra.runtime_completeness,
        }
    }
}

impl ShellPollingSummary {
    fn new(steps: &[AtifStep]) -> Self {
        let mut poll_only_steps = 0_usize;
        let model_call_attribution_complete = steps
            .iter()
            .filter(|step| matches!(step.source, AtifSource::Agent))
            .all(|step| step.llm_call_count.is_some());
        let mut confirmed_model_calls = model_call_attribution_complete.then_some(0_u32);
        let mut empty_stdin_tool_calls = 0_usize;
        let mut sessions = BTreeSet::new();
        let mut explicit_requested_yield_ms = 0_u64;
        let mut tool_wait_duration_ns = 0_u64;
        let mut model_duration_ns = 0_u64;
        let mut prompt_tokens = 0_u64;
        let mut cached_tokens = 0_u64;
        let mut completion_tokens = 0_u64;

        for step in steps {
            let Some(tool_calls) = step.tool_calls.as_deref() else {
                continue;
            };
            let polling_calls = tool_calls
                .iter()
                .filter_map(|tool_call| {
                    empty_write_stdin_arguments(tool_call).map(|arguments| (tool_call, arguments))
                })
                .collect::<Vec<_>>();
            if polling_calls.is_empty()
                || !tool_calls.iter().all(|tool_call| {
                    tool_call.function_name == "exec"
                        || empty_write_stdin_arguments(tool_call).is_some()
                })
            {
                continue;
            }

            poll_only_steps = poll_only_steps.saturating_add(1);
            confirmed_model_calls = confirmed_model_calls
                .zip(step.llm_call_count)
                .map(|(total, count)| total.saturating_add(count));
            empty_stdin_tool_calls = empty_stdin_tool_calls.saturating_add(polling_calls.len());

            for (tool_call, arguments) in polling_calls {
                if let Some(session_id) = arguments.get("session_id") {
                    sessions.insert(session_id.to_string());
                }
                explicit_requested_yield_ms = explicit_requested_yield_ms.saturating_add(
                    arguments
                        .get("yield_time_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default(),
                );
                let duration_ns = step
                    .observation
                    .as_ref()
                    .and_then(|observation| {
                        observation
                            .results
                            .iter()
                            .find(|result| result.source_call_id == tool_call.tool_call_id)
                    })
                    .map_or(0, |result| result.extra.duration_ns);
                tool_wait_duration_ns = tool_wait_duration_ns.saturating_add(duration_ns);
            }

            if let Some(metrics) = &step.metrics {
                model_duration_ns = model_duration_ns.saturating_add(metrics.extra.duration_ns);
                prompt_tokens = prompt_tokens.saturating_add(metrics.prompt_tokens);
                cached_tokens = cached_tokens.saturating_add(metrics.cached_tokens);
                completion_tokens = completion_tokens.saturating_add(metrics.completion_tokens);
            }
        }

        Self {
            poll_only_steps: count_u32(poll_only_steps),
            model_call_attribution_complete,
            confirmed_model_calls,
            empty_stdin_tool_calls: count_u32(empty_stdin_tool_calls),
            sessions: count_u32(sessions.len()),
            explicit_requested_yield_ms,
            tool_wait_duration_ns,
            model_duration_ns,
            prompt_tokens,
            cached_tokens,
            completion_tokens,
        }
    }
}

fn empty_write_stdin_arguments(tool_call: &AtifToolCall) -> Option<serde_json::Value> {
    if tool_call.function_name != "write_stdin" {
        return None;
    }
    let arguments = serde_json::from_str::<serde_json::Value>(tool_call.arguments.get()).ok()?;
    match arguments.get("chars") {
        Some(serde_json::Value::String(chars)) if chars.is_empty() => Some(arguments),
        None => Some(arguments),
        _ => None,
    }
}

impl TrajectoryComparison {
    const fn unavailable() -> Self {
        Self {
            comparable: false,
            tool_sequence_comparable: false,
            tool_sequence_equal: None,
            codex_minus_nanocodex: None,
        }
    }

    fn from_arms(nanocodex: &ArmReport, codex: &ArmReport) -> Self {
        let (Some(nanocodex), Some(codex)) = (
            nanocodex.trajectory_summary.as_ref(),
            codex.trajectory_summary.as_ref(),
        ) else {
            return Self::unavailable();
        };
        Self::from_summaries(nanocodex, codex)
    }

    fn from_summaries(nanocodex: &TrajectorySummary, codex: &TrajectorySummary) -> Self {
        let tool_sequence_comparable = codex.tool_projection == nanocodex.tool_projection;
        Self {
            comparable: true,
            tool_sequence_comparable,
            tool_sequence_equal: tool_sequence_comparable
                .then(|| codex.tool_sequence == nanocodex.tool_sequence),
            codex_minus_nanocodex: Some(TrajectoryDelta {
                total_steps: i64::from(codex.total_steps) - i64::from(nanocodex.total_steps),
                agent_steps: i64::from(codex.agent_steps) - i64::from(nanocodex.agent_steps),
                message_steps: i64::from(codex.message_steps) - i64::from(nanocodex.message_steps),
                reasoning_steps: i64::from(codex.reasoning_steps)
                    - i64::from(nanocodex.reasoning_steps),
                tool_calls: tool_sequence_comparable
                    .then(|| i64::from(codex.tool_calls) - i64::from(nanocodex.tool_calls)),
                observations: tool_sequence_comparable
                    .then(|| i64::from(codex.observations) - i64::from(nanocodex.observations)),
                model_calls: codex
                    .model_calls
                    .zip(nanocodex.model_calls)
                    .map(|(codex, nanocodex)| i64::from(codex) - i64::from(nanocodex)),
                shell_polling: ShellPollingDelta::between(
                    &codex.shell_polling,
                    &nanocodex.shell_polling,
                ),
            }),
        }
    }
}

impl ShellPollingDelta {
    fn between(codex: &ShellPollingSummary, nanocodex: &ShellPollingSummary) -> Self {
        Self {
            poll_only_steps: i64::from(codex.poll_only_steps)
                - i64::from(nanocodex.poll_only_steps),
            confirmed_model_calls: codex
                .confirmed_model_calls
                .zip(nanocodex.confirmed_model_calls)
                .map(|(codex, nanocodex)| i64::from(codex) - i64::from(nanocodex)),
            empty_stdin_tool_calls: i64::from(codex.empty_stdin_tool_calls)
                - i64::from(nanocodex.empty_stdin_tool_calls),
            sessions: i64::from(codex.sessions) - i64::from(nanocodex.sessions),
            explicit_requested_yield_ms: signed_u64_delta(
                codex.explicit_requested_yield_ms,
                nanocodex.explicit_requested_yield_ms,
            ),
            tool_wait_duration_ns: signed_u64_delta(
                codex.tool_wait_duration_ns,
                nanocodex.tool_wait_duration_ns,
            ),
            model_duration_ns: signed_u64_delta(
                codex.model_duration_ns,
                nanocodex.model_duration_ns,
            ),
            prompt_tokens: signed_u64_delta(codex.prompt_tokens, nanocodex.prompt_tokens),
            cached_tokens: signed_u64_delta(codex.cached_tokens, nanocodex.cached_tokens),
            completion_tokens: signed_u64_delta(
                codex.completion_tokens,
                nanocodex.completion_tokens,
            ),
        }
    }
}

impl ArmReport {
    fn from_outcome(
        evaluator_directory: PathBuf,
        event_log: PathBuf,
        outcome: EvalAttemptOutcome,
        event_error: Option<String>,
        trajectory: InternalResult<TrajectoryArtifact, String>,
        codex_artifacts: bool,
        api_capture_required: bool,
    ) -> Self {
        let attempt_directory = outcome_directory(&outcome);
        let (codex_events, codex_stderr, codex_summary) = if codex_artifacts {
            (
                retained_file(attempt_directory.join("agent/codex-events.jsonl")),
                retained_file(attempt_directory.join("agent/codex-stderr.log")),
                retained_file(attempt_directory.join("agent/codex-summary.json")),
            )
        } else {
            (None, None, None)
        };
        let (trajectory, trajectory_summary, trajectory_error) = match trajectory {
            Ok(artifact) => (Some(artifact.path), Some(artifact.summary), None),
            Err(error) => (None, None, Some(error)),
        };
        let api_capture = retain_arm_api_exchanges(
            &event_log,
            attempt_directory,
            codex_artifacts,
            api_capture_required,
        );
        let (api_exchanges, api_capture, api_capture_error) = match api_capture {
            Ok(Some(artifact)) => (Some(artifact.path), Some(artifact.summary), None),
            Ok(None) => (None, None, None),
            Err(error) => (None, None, Some(format!("{error:#}"))),
        };
        Self {
            summary: ArmSummary::from_outcome(&outcome),
            evaluator_directory: Some(evaluator_directory),
            event_log: retained_file(event_log),
            trajectory,
            trajectory_summary,
            trajectory_error,
            api_exchanges,
            api_capture,
            api_capture_error,
            codex_events,
            codex_stderr,
            codex_summary,
            operational_error: None,
            event_error,
            memory: None,
            outcome: Some(outcome),
        }
    }

    fn runner_error(
        evaluator_directory: PathBuf,
        event_log: PathBuf,
        error: String,
        event_error: Option<String>,
    ) -> Self {
        Self {
            summary: ArmSummary::runner_error(),
            evaluator_directory: Some(evaluator_directory),
            event_log: retained_file(event_log),
            trajectory: None,
            trajectory_summary: None,
            trajectory_error: None,
            api_exchanges: None,
            api_capture: None,
            api_capture_error: None,
            codex_events: None,
            codex_stderr: None,
            codex_summary: None,
            operational_error: Some(error),
            event_error,
            memory: None,
            outcome: None,
        }
    }

    const fn setup_error(error: String) -> Self {
        Self {
            summary: ArmSummary::runner_error(),
            evaluator_directory: None,
            event_log: None,
            trajectory: None,
            trajectory_summary: None,
            trajectory_error: None,
            api_exchanges: None,
            api_capture: None,
            api_capture_error: None,
            codex_events: None,
            codex_stderr: None,
            codex_summary: None,
            operational_error: Some(error),
            event_error: None,
            memory: None,
            outcome: None,
        }
    }
}

impl From<VmAttemptMemorySnapshot> for ArmMemoryReport {
    fn from(memory: VmAttemptMemorySnapshot) -> Self {
        Self {
            host_peak_rss_mib: memory.host_peak_rss_mib(),
            guest_total_mib: memory.guest_total_mib(),
            guest_peak_used_mib: memory.guest_peak_used_mib(),
            guest_oom_kills: memory.guest_oom_kills(),
            oom_detected: memory.oom_detected(),
        }
    }
}

impl ArmSummary {
    const fn is_infrastructure_failure(&self) -> bool {
        matches!(self.outcome, Some(EvalOutcome::InfrastructureError))
    }

    fn apply_model_visible_tool_calls(&mut self, summary: Option<&ApiEventLoopArmSummary>) {
        self.tool_calls = summary.map(|summary| summary.model_visible_tool_calls);
    }

    fn from_outcome(outcome: &EvalAttemptOutcome) -> Self {
        match outcome {
            EvalAttemptOutcome::Scored(result) => Self {
                status: match result.status {
                    EvalStatus::Passed => ArmStatus::Passed,
                    EvalStatus::Failed => ArmStatus::VerifierFailed,
                },
                outcome: Some(result.outcome),
                exception: result.exception.as_ref().map(|exception| exception.kind),
                verifier_exit_code: Some(result.verifier.exit_code),
                rewards: result.verifier.rewards.clone(),
                model: result.agent.as_ref().map(|agent| agent.model.clone()),
                tool_calls: None,
                tool_call_measurement: MODEL_VISIBLE_TOOL_CALL_MEASUREMENT,
                observed_tool_events: result.agent.as_ref().map(|agent| agent.tool_calls),
                usage: result.agent.as_ref().map(|agent| agent.usage.clone()),
                duration_ms: result.agent.as_ref().map(agent_duration_ms),
            },
            EvalAttemptOutcome::Unscored(failure) => Self {
                status: ArmStatus::Unscored,
                outcome: Some(failure.exception.outcome),
                exception: Some(failure.exception.kind),
                verifier_exit_code: failure.verifier.as_ref().map(|verifier| verifier.exit_code),
                rewards: failure
                    .verifier
                    .as_ref()
                    .map_or_else(BTreeMap::new, |verifier| verifier.rewards.clone()),
                model: failure.agent.as_ref().map(|agent| agent.model.clone()),
                tool_calls: None,
                tool_call_measurement: MODEL_VISIBLE_TOOL_CALL_MEASUREMENT,
                observed_tool_events: failure.agent.as_ref().map(|agent| agent.tool_calls),
                usage: failure.agent.as_ref().map(|agent| agent.usage.clone()),
                duration_ms: failure.agent.as_ref().map(agent_duration_ms),
            },
        }
    }

    const fn runner_error() -> Self {
        Self {
            status: ArmStatus::RunnerError,
            outcome: None,
            exception: None,
            verifier_exit_code: None,
            rewards: BTreeMap::new(),
            model: None,
            tool_calls: None,
            tool_call_measurement: MODEL_VISIBLE_TOOL_CALL_MEASUREMENT,
            observed_tool_events: None,
            usage: None,
            duration_ms: None,
        }
    }
}

async fn run_arm(
    task: Task,
    evaluator: EvaluatorBuilder,
    projection: TrajectoryProjection,
    api_capture_required: bool,
    progress: DiffProgress,
) -> ArmReport {
    let codex_artifacts = matches!(&projection, TrajectoryProjection::Codex { .. });
    let arm_name = if codex_artifacts {
        "codex"
    } else {
        "nanocodex"
    };
    progress.emit(arm_name, "attempt.started", task.name());
    let evaluator = match evaluator.build() {
        Ok(built) => built,
        Err(error) => {
            let report = ArmReport::setup_error(format!("{error:#}"));
            progress.emit(
                arm_name,
                "attempt.failed",
                report
                    .operational_error
                    .as_deref()
                    .unwrap_or("evaluator setup failed"),
            );
            return report;
        }
    };
    let evaluator_directory = evaluator.directory().to_path_buf();
    let event_log = evaluator_directory.join("events.jsonl");
    let run = evaluator.task(task);
    let stream = run.events().subscribe();
    let event_path = event_log.clone();
    let event_progress = progress.clone();
    let event_recorder =
        tokio::spawn(
            async move { record_events(stream, &event_path, arm_name, event_progress).await },
        );
    let outcome = run.await;
    let (recording, event_error) = match event_recorder.await {
        Ok(Ok(recording)) => (Some(recording), None),
        Ok(Err(error)) => (None, Some(format!("{error:#}"))),
        Err(error) => (None, Some(format!("event recorder task failed: {error}"))),
    };
    let report = match outcome {
        Ok(outcome) => {
            let trajectory = recording.map_or_else(
                || {
                    Err("trajectory projection unavailable because evaluator event recording failed"
                        .to_owned())
                },
                |recording| {
                    retain_trajectory(&outcome, recording, projection)
                        .map_err(|error| format!("{error:#}"))
                },
            );
            ArmReport::from_outcome(
                evaluator_directory,
                event_log,
                outcome,
                event_error,
                trajectory,
                codex_artifacts,
                api_capture_required,
            )
        }
        Err(error) => ArmReport::runner_error(
            evaluator_directory,
            event_log,
            format!("{error:#}"),
            event_error,
        ),
    };
    progress.emit(
        arm_name,
        "attempt.completed",
        format!(
            "{} · reward {}",
            report.summary.status.as_str(),
            report
                .summary
                .rewards
                .values()
                .next()
                .map_or_else(|| "unscored".to_owned(), ToString::to_string)
        ),
    );
    report
}

async fn record_events(
    mut stream: EvalEventStream,
    path: &Path,
    arm_name: &'static str,
    progress: DiffProgress,
) -> InternalResult<EventRecording> {
    let mut output = tokio::fs::File::create(path)
        .await
        .wrap_err_with(|| format!("failed to create evaluator event log {}", path.display()))?;
    let mut atif = AtifBuilder::default();
    let mut atif_error = None;
    while let Some(event) = stream.recv().await? {
        progress.observe_evaluator(arm_name, &event.kind);
        if let EvalEventKind::Agent(agent_event) = &event.kind {
            let payload = serde_json::from_str(agent_event.payload.get()).unwrap_or_default();
            if matches!(agent_event.kind, AgentEventKind::ApiEvent) && arm_name == "nanocodex" {
                progress.observe_nanocodex_api(&payload);
            } else if !matches!(
                agent_event.kind,
                AgentEventKind::AssistantDelta | AgentEventKind::ReasoningSummaryDelta
            ) && arm_name == "nanocodex"
            {
                progress.observe_nanocodex(agent_event);
            }
            if atif_error.is_none()
                && let Err(error) = atif.apply(agent_event)
            {
                atif_error = Some(format!(
                    "failed to project agent event sequence {} into ATIF: {error}",
                    event.sequence
                ));
            }
        }
        let mut encoded = serde_json::to_vec(event.as_ref())?;
        encoded.push(b'\n');
        output.write_all(&encoded).await?;
    }
    output.flush().await?;
    output.sync_all().await?;
    Ok(EventRecording { atif, atif_error })
}

fn retain_trajectory(
    outcome: &EvalAttemptOutcome,
    recording: EventRecording,
    projection: TrajectoryProjection,
) -> InternalResult<TrajectoryArtifact> {
    let task = outcome_task(outcome);
    let trajectory = match projection {
        TrajectoryProjection::Nanocodex => {
            if let Some(error) = recording.atif_error {
                return Err(diff_error!(error));
            }
            match outcome_agent(outcome) {
                Some(agent) => recording.atif.finish(task, agent),
                None => recording.atif.finish_failure(task),
            }
        }
        TrajectoryProjection::Codex { version } => {
            let agent = outcome_agent(outcome).ok_or_else(|| {
                diff_error!("stock Codex attempt retained no terminal agent result")
            })?;
            let events = outcome_directory(outcome).join("agent/codex-events.jsonl");
            let version = version.resolve()?;
            project_codex_atif(&events, task.prompt(), agent, &version).wrap_err_with(|| {
                format!("failed to project stock Codex stream {}", events.display())
            })?
        }
    };
    let path = outcome_directory(outcome).join(TRAJECTORY_FILE);
    let parent = path
        .parent()
        .ok_or_else(|| diff_error!("trajectory path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .wrap_err_with(|| format!("failed to create trajectory directory {}", parent.display()))?;
    let summary = TrajectorySummary::new(&trajectory);
    write_json_atomic(&path, &trajectory)?;
    Ok(TrajectoryArtifact { path, summary })
}

mod api_analysis;
use api_analysis::*;

/// Rebuilds derived trajectory and API comparisons from retained raw evidence.
///
/// This performs no agent, model, VM, or verifier work.
///
/// # Errors
///
/// Returns an error when the retained comparison is malformed, a referenced
/// artifact is missing, or the rebuilt evidence cannot be published.
pub fn reanalyze(path: impl AsRef<Path>) -> DifferentialResult<DifferentialReanalysis> {
    reanalyze_inner(path.as_ref()).map_err(DifferentialError::new)
}

fn reanalyze_inner(path: &Path) -> InternalResult<DifferentialReanalysis> {
    let requested = path
        .canonicalize()
        .wrap_err_with(|| format!("failed to resolve retained comparison {}", path.display()))?;
    let comparison_path = if requested.is_dir() {
        requested.join(COMPARISON_FILE)
    } else {
        requested
    };
    let directory = comparison_path.parent().ok_or_else(|| {
        diff_error!(
            "retained comparison has no parent directory: {}",
            comparison_path.display()
        )
    })?;
    let mut comparison: serde_json::Value =
        serde_json::from_reader(File::open(&comparison_path).wrap_err_with(|| {
            format!(
                "failed to open retained comparison {}",
                comparison_path.display()
            )
        })?)
        .wrap_err_with(|| {
            format!(
                "retained comparison is not valid JSON: {}",
                comparison_path.display()
            )
        })?;
    let nanocodex_trajectory_summary =
        retained_trajectory_summary(&comparison, directory, "/nanocodex/trajectory", "Nanocodex")?;
    let codex_trajectory_summary =
        retained_trajectory_summary(&comparison, directory, "/codex/trajectory", "Codex")?;
    let trajectory_comparison = match (
        nanocodex_trajectory_summary.as_ref(),
        codex_trajectory_summary.as_ref(),
    ) {
        (Some(nanocodex), Some(codex)) => TrajectoryComparison::from_summaries(nanocodex, codex),
        _ => TrajectoryComparison::unavailable(),
    };

    let nanocodex_api_path = retained_artifact_path(
        &comparison,
        directory,
        "/nanocodex/api_exchanges",
        "Nanocodex",
        "API exchange capture",
    )?;
    let codex_api_path = retained_artifact_path(
        &comparison,
        directory,
        "/codex/api_exchanges",
        "Codex",
        "API exchange capture",
    )?;
    let api_comparison_path = comparison
        .pointer("/artifacts/api_comparison")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .map_or_else(
            || directory.join(API_COMPARISON_FILE),
            |path| {
                if path.is_absolute() {
                    path
                } else {
                    directory.join(path)
                }
            },
        );
    let api_summary = match (nanocodex_api_path.as_ref(), codex_api_path.as_ref()) {
        (Some(nanocodex_path), Some(codex_path)) => {
            let nanocodex_capture = inspect_api_exchanges(
                nanocodex_path.clone(),
                "responses_request_and_response_payloads",
                "complete_observed_json_values",
            )?
            .summary;
            let codex_capture = inspect_api_exchanges(
                codex_path.clone(),
                "all_api_payloads_routed_through_configured_base_url",
                "exact_wire_payload_bytes",
            )?
            .summary;
            Some(compare_api_exchanges(
                &api_comparison_path,
                Some(nanocodex_path),
                Some(codex_path),
                Some(nanocodex_capture),
                Some(codex_capture),
            )?)
        }
        _ => None,
    };
    let expected_model = comparison
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let expected_effort = comparison
        .get("thinking")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let profile_validation_error = if let Some(summary) = api_summary.as_ref() {
        let nanocodex_tool_mode = retained_nanocodex_tool_mode(&comparison)?;
        let codex_tool_mode = retained_codex_tool_mode(&comparison)?;
        let web_search = comparison
            .pointer("/policy/web_search")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        expected_model
            .as_deref()
            .zip(expected_effort.as_deref())
            .and_then(|(model, effort)| {
                validate_differential_profile(
                    summary,
                    model,
                    effort,
                    nanocodex_tool_mode,
                    codex_tool_mode,
                    web_search,
                )
            })
    } else {
        None
    };

    let comparison_object = comparison
        .as_object_mut()
        .ok_or_else(|| diff_error!("retained comparison root is not an object"))?;
    comparison_object.insert(
        "trajectory_comparison".to_owned(),
        serde_json::to_value(&trajectory_comparison)?,
    );
    let nanocodex = comparison_object
        .get_mut("nanocodex")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| diff_error!("retained comparison has no Nanocodex arm object"))?;
    normalize_retained_arm_tool_calls(
        nanocodex,
        api_summary
            .as_ref()
            .and_then(|summary| summary.event_loop.nanocodex.as_ref()),
    );
    nanocodex.insert(
        "trajectory_summary".to_owned(),
        serde_json::to_value(&nanocodex_trajectory_summary)?,
    );
    let codex = comparison_object
        .get_mut("codex")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| diff_error!("retained comparison has no Codex arm object"))?;
    normalize_retained_arm_tool_calls(
        codex,
        api_summary
            .as_ref()
            .and_then(|summary| summary.event_loop.codex.as_ref()),
    );
    codex.insert(
        "trajectory_summary".to_owned(),
        serde_json::to_value(&codex_trajectory_summary)?,
    );
    comparison_object.insert(
        "api_comparison".to_owned(),
        serde_json::to_value(
            api_summary
                .clone()
                .unwrap_or_else(ApiComparisonSummary::unavailable),
        )?,
    );
    let artifacts = comparison_object
        .get_mut("artifacts")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| diff_error!("retained comparison has no artifacts object"))?;
    artifacts.insert(
        "profile_validation_error".to_owned(),
        serde_json::to_value(&profile_validation_error)?,
    );
    write_json_atomic(&comparison_path, &comparison)?;

    let rebuilt = if api_summary.is_some() {
        serde_json::from_reader(File::open(&api_comparison_path)?)?
    } else {
        comparison
    };
    let mut human_summary = String::new();
    let _ = writeln!(
        human_summary,
        "reanalyzed retained evidence{} without running either agent",
        if api_summary.is_some() {
            " and API captures"
        } else {
            "; API captures unavailable"
        }
    );
    if let Some(summary) = &nanocodex_trajectory_summary {
        append_shell_polling_summary(&mut human_summary, "nanocodex", &summary.shell_polling);
    } else {
        let _ = writeln!(human_summary, "nanocodex trajectory: unavailable");
    }
    if let Some(summary) = &codex_trajectory_summary {
        append_shell_polling_summary(&mut human_summary, "codex", &summary.shell_polling);
    } else {
        let _ = writeln!(human_summary, "codex trajectory: unavailable");
    }
    if let Some(summary) = &api_summary {
        let _ = writeln!(
            human_summary,
            "event loop: chain invariants {} · {} aligned, {} matching, {} differing · unpaired nanocodex {} / codex {}",
            summary
                .event_loop
                .chain_invariants_equal
                .map_or("unavailable", |equal| if equal {
                    "match"
                } else {
                    "differ"
                }),
            summary.event_loop.aligned_turns,
            summary.event_loop.equal_turns,
            summary.event_loop.differing_turns,
            summary.event_loop.nanocodex_unpaired_turns,
            summary.event_loop.codex_unpaired_turns,
        );
        if let Some(divergence) = &summary.event_loop.first_divergence {
            let _ = writeln!(
                human_summary,
                "first event-loop divergence: turn {} · {} · {}",
                divergence.request_index,
                divergence.categories.join(","),
                divergence.pointer
            );
        }
        append_first_generation_divergence(&mut human_summary, &summary.event_loop);
        append_event_loop_arm_summary(
            &mut human_summary,
            "nanocodex",
            summary.event_loop.nanocodex.as_ref(),
        );
        append_event_loop_arm_summary(
            &mut human_summary,
            "codex",
            summary.event_loop.codex.as_ref(),
        );
        append_unpaired_tail_summary(&mut human_summary, &summary.event_loop);
        if let Some(error) = &profile_validation_error {
            let _ = writeln!(human_summary, "matched-profile error: {error}");
        }
        let _ = writeln!(
            human_summary,
            "API comparison: {}",
            api_comparison_path.display()
        );
    }
    let _ = writeln!(human_summary, "comparison: {}", comparison_path.display());
    Ok(DifferentialReanalysis {
        comparison: rebuilt,
        comparison_path,
        api_comparison_path: api_summary.is_some().then_some(api_comparison_path),
        human_summary,
    })
}

fn retained_artifact_path(
    comparison: &serde_json::Value,
    directory: &Path,
    pointer: &str,
    arm: &str,
    artifact: &str,
) -> InternalResult<Option<PathBuf>> {
    let Some(retained) = comparison
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    let retained = PathBuf::from(retained);
    let resolved = if retained.is_absolute() {
        retained
    } else {
        directory.join(retained)
    };
    if !resolved.is_file() {
        return Err(diff_error!(
            "{arm} {artifact} is not a file: {}",
            resolved.display()
        ));
    }
    Ok(Some(resolved))
}

fn retained_trajectory_summary(
    comparison: &serde_json::Value,
    directory: &Path,
    pointer: &str,
    arm: &str,
) -> InternalResult<Option<TrajectorySummary>> {
    let Some(path) = retained_artifact_path(comparison, directory, pointer, arm, "trajectory")?
    else {
        return Ok(None);
    };
    let trajectory: AtifTrajectory =
        serde_json::from_reader(File::open(&path).wrap_err_with(|| {
            format!(
                "failed to open retained {arm} trajectory {}",
                path.display()
            )
        })?)?;
    Ok(Some(TrajectorySummary::new(&trajectory)))
}

fn normalize_retained_arm_tool_calls(
    arm: &mut serde_json::Map<String, serde_json::Value>,
    api: Option<&ApiEventLoopArmSummary>,
) {
    let Some(summary) = arm
        .get_mut("summary")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if !summary.contains_key("observed_tool_events") {
        let observed = summary
            .get("tool_calls")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        summary.insert("observed_tool_events".to_owned(), observed);
    }
    summary.insert(
        "tool_calls".to_owned(),
        api.map_or(serde_json::Value::Null, |summary| {
            serde_json::Value::from(summary.model_visible_tool_calls)
        }),
    );
    summary.insert(
        "tool_call_measurement".to_owned(),
        serde_json::Value::from(MODEL_VISIBLE_TOOL_CALL_MEASUREMENT),
    );
}

fn append_shell_polling_summary(output: &mut String, name: &str, summary: &ShellPollingSummary) {
    let _ = writeln!(
        output,
        "{name} shell polling: {} observed poll-only steps · {} confirmed poll-only model calls · {} sessions · {} input/{} output tokens · {:.1}s model time",
        summary.poll_only_steps,
        summary
            .confirmed_model_calls
            .map_or_else(|| "unavailable".to_owned(), |calls| calls.to_string()),
        summary.sessions,
        summary.prompt_tokens,
        summary.completion_tokens,
        Duration::from_nanos(summary.model_duration_ns).as_secs_f64(),
    );
}

fn append_event_loop_arm_summary(
    output: &mut String,
    name: &str,
    summary: Option<&ApiEventLoopArmSummary>,
) {
    let Some(summary) = summary else {
        let _ = writeln!(output, "{name} event loop: unavailable");
        return;
    };
    let _ = writeln!(
        output,
        "{name} event loop: {}/{} terminal · {} generation turns · model-visible calls {} [{}] · captured usage {} total tokens ({} cached + {} uncached input, {} output, {} reasoning) on {}/{} turns · profile {}/{}/summary={} · visible tools [{}] · {} detected poll-only ({} empty stdin calls; {} explicit yields totaling {}ms) · previous links {} direct/{} replay ({} after nonterminal)/{} broken · tool-result links {} valid/{} replayed/{} broken · cache stable {}",
        summary.terminal_turns,
        summary.turns,
        summary.generation_turns,
        summary.model_visible_tool_calls,
        summary.model_visible_tool_sequence.join(", "),
        summary.usage.total_tokens,
        summary.usage.cached_input_tokens,
        summary.usage.uncached_input_tokens,
        summary.usage.output_tokens,
        summary.usage.reasoning_output_tokens,
        summary.turns_with_usage,
        summary.turns,
        summary.initial_model.as_deref().unwrap_or("unobserved"),
        summary
            .initial_reasoning_effort
            .as_deref()
            .unwrap_or("unobserved"),
        summary
            .initial_reasoning_summary
            .as_deref()
            .unwrap_or("unobserved"),
        summary.initial_visible_tools.join(", "),
        summary.detected_poll_only_turns,
        summary.detected_empty_stdin_calls,
        summary.detected_polling_calls_with_explicit_yield,
        summary.detected_polling_explicit_yield_ms,
        summary.previous_response_links,
        summary.full_history_replays,
        summary.full_history_replays_after_nonterminal_turn,
        summary.broken_previous_response_links,
        summary.tool_result_links,
        summary.replayed_tool_result_links,
        summary.broken_tool_result_links,
        summary
            .prompt_cache_key_stable
            .map_or("unobserved", |stable| if stable { "yes" } else { "no" }),
    );
}

fn append_model_visible_tool_summary(output: &mut String, comparison: &ApiEventLoopComparison) {
    let (Some(nanocodex), Some(codex)) = (comparison.nanocodex.as_ref(), comparison.codex.as_ref())
    else {
        return;
    };
    let format_input_text_sections = |sections: &[ApiInputTextSectionSummary]| {
        sections
            .iter()
            .map(|section| format!("{}/{}:{}B", section.role, section.label, section.text_bytes))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = writeln!(
        output,
        "initial model input text: nanocodex [{}] · codex [{}] · match {}",
        format_input_text_sections(&nanocodex.initial_input_text_sections),
        format_input_text_sections(&codex.initial_input_text_sections),
        comparison
            .initial_input_text_sections_equal
            .map_or("unavailable", |equal| if equal { "yes" } else { "no" }),
    );
    let _ = writeln!(
        output,
        "initial generation input text: nanocodex [{}] · codex [{}] · match {}",
        format_input_text_sections(&nanocodex.initial_generation_input_text_sections),
        format_input_text_sections(&codex.initial_generation_input_text_sections),
        comparison
            .initial_generation_input_text_sections_equal
            .map_or("unavailable", |equal| if equal { "yes" } else { "no" }),
    );
    let _ = writeln!(
        output,
        "initial complete model-visible tool definitions match: {}",
        comparison
            .initial_visible_tool_definitions_equal
            .map_or("unavailable", |equal| if equal { "yes" } else { "no" }),
    );
    let _ = writeln!(
        output,
        "initial generation complete model-visible tool definitions match: {}",
        comparison
            .initial_generation_visible_tool_definitions_equal
            .map_or("unavailable", |equal| if equal { "yes" } else { "no" }),
    );
    let _ = writeln!(
        output,
        "model-visible tool sequence: nanocodex [{}] · codex [{}] · match {}",
        nanocodex.model_visible_tool_sequence.join(", "),
        codex.model_visible_tool_sequence.join(", "),
        comparison
            .model_visible_tool_sequence_equal
            .map_or("unavailable", |equal| if equal { "yes" } else { "no" }),
    );
    let format_code_mode_tools = |tools: Option<&[String]>| {
        tools.map_or_else(
            || "unavailable".to_owned(),
            |tools| format!("[{}]", tools.join(", ")),
        )
    };
    let _ = writeln!(
        output,
        "nested Code Mode tool catalog: nanocodex {} · codex {} · match {}",
        format_code_mode_tools(nanocodex.initial_code_mode_tools.as_deref()),
        format_code_mode_tools(codex.initial_code_mode_tools.as_deref()),
        comparison
            .initial_code_mode_tool_names_equal
            .map_or("unavailable", |equal| if equal { "yes" } else { "no" }),
    );
    let _ = writeln!(
        output,
        "nested Code Mode tool definitions match: {}",
        comparison
            .initial_code_mode_tool_definitions_equal
            .map_or("unavailable", |equal| if equal { "yes" } else { "no" }),
    );
}

fn append_first_generation_divergence(output: &mut String, comparison: &ApiEventLoopComparison) {
    let Some(divergence) = &comparison.first_generation_divergence else {
        return;
    };
    let _ = writeln!(
        output,
        "first generation divergence: turn {} · {} · {}",
        divergence.request_index,
        divergence.categories.join(","),
        divergence.pointer,
    );
}

fn append_unpaired_tail_summary(output: &mut String, comparison: &ApiEventLoopComparison) {
    let (Some(nanocodex), Some(codex)) = (
        comparison.nanocodex_unpaired_tail.as_ref(),
        comparison.codex_unpaired_tail.as_ref(),
    ) else {
        return;
    };
    if nanocodex.turns == 0 && codex.turns == 0 {
        return;
    }
    let format = |tail: &ApiEventLoopTailSummary| {
        format!(
            "{} turns/{} generation/{} poll-only ({} calls; {} explicit yields totaling {}ms) · {} total tokens ({} cached + {} uncached input, {} output, {} reasoning) · usage {}/{}",
            tail.turns,
            tail.generation_turns,
            tail.detected_poll_only_turns,
            tail.detected_empty_stdin_calls,
            tail.detected_polling_calls_with_explicit_yield,
            tail.detected_polling_explicit_yield_ms,
            tail.usage.total_tokens,
            tail.usage.cached_input_tokens,
            tail.usage.uncached_input_tokens,
            tail.usage.output_tokens,
            tail.usage.reasoning_output_tokens,
            tail.turns_with_usage,
            tail.turns,
        )
    };
    let _ = writeln!(
        output,
        "unpaired API tail: nanocodex [{}] · codex [{}]",
        format(nanocodex),
        format(codex),
    );
}

fn prepare_output_parent(output: &Path) -> InternalResult<PathBuf> {
    fs::create_dir_all(output)
        .wrap_err_with(|| format!("failed to create output directory {}", output.display()))?;
    output
        .canonicalize()
        .wrap_err_with(|| format!("failed to resolve output directory {}", output.display()))
}

fn outcome_directory(outcome: &EvalAttemptOutcome) -> &Path {
    match outcome {
        EvalAttemptOutcome::Scored(result) => &result.artifacts.directory,
        EvalAttemptOutcome::Unscored(failure) => &failure.artifacts.directory,
    }
}

const fn outcome_task(outcome: &EvalAttemptOutcome) -> &Task {
    match outcome {
        EvalAttemptOutcome::Scored(result) => result.task(),
        EvalAttemptOutcome::Unscored(failure) => failure.task(),
    }
}

const fn outcome_agent(outcome: &EvalAttemptOutcome) -> Option<&AgentResult> {
    match outcome {
        EvalAttemptOutcome::Scored(result) => result.agent.as_ref(),
        EvalAttemptOutcome::Unscored(failure) => failure.agent.as_ref(),
    }
}

fn retained_file(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

fn count_u32(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn signed_u64_delta(left: u64, right: u64) -> i64 {
    i64::try_from(left)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(right).unwrap_or(i64::MAX))
}

const fn agent_duration_ms(agent: &AgentResult) -> u64 {
    agent.metadata.duration_ms
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn file_sha256(path: &Path) -> InternalResult<String> {
    let mut file =
        File::open(path).wrap_err_with(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .wrap_err_with(|| format!("failed to hash {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> InternalResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| diff_error!("comparison path has no parent: {}", path.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .wrap_err_with(|| format!("failed to create temporary file in {}", parent.display()))?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), value)?;
    temporary.as_file_mut().write_all(b"\n")?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .wrap_err_with(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

fn append_arm_summary(output: &mut String, name: &str, arm: &ArmReport) {
    let status = arm.summary.status.as_str();
    let reward = arm
        .summary
        .rewards
        .iter()
        .map(|(name, reward)| format!("{name}={reward}"))
        .collect::<Vec<_>>()
        .join(",");
    let model_visible_tools = arm
        .summary
        .tool_calls
        .map_or_else(|| "unknown".to_owned(), |calls| calls.to_string());
    let observed_tool_events = arm
        .summary
        .observed_tool_events
        .map_or_else(|| "unknown".to_owned(), |calls| calls.to_string());
    if reward.is_empty() {
        let _ = writeln!(
            output,
            "{name}: {status} model_visible_tool_calls={model_visible_tools} observed_tool_events={observed_tool_events}"
        );
    } else {
        let _ = writeln!(
            output,
            "{name}: {status} {reward} model_visible_tool_calls={model_visible_tools} observed_tool_events={observed_tool_events}"
        );
    }
    if let Some(memory) = arm.memory {
        let _ = writeln!(
            output,
            "{name} memory: host_peak={} MiB · guest_peak={} MiB / total={} MiB · oom={}",
            memory
                .host_peak_rss_mib
                .map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
            memory
                .guest_peak_used_mib
                .map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
            memory
                .guest_total_mib
                .map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
            memory.oom_detected,
        );
    }
    if let Some(trajectory) = &arm.trajectory {
        let _ = writeln!(output, "{name} trajectory: {}", trajectory.display());
    }
    if let Some(summary) = &arm.trajectory_summary {
        let polling = &summary.shell_polling;
        let _ = writeln!(
            output,
            "{name} trajectory tools: {} [{}] ({})",
            summary.tool_calls,
            summary.tool_sequence.join(", "),
            summary.tool_projection,
        );
        let _ = writeln!(
            output,
            "{name} shell polling: {} observed poll-only steps · {} confirmed poll-only model calls · {} input/{} output tokens · {:.1}s model time",
            polling.poll_only_steps,
            polling
                .confirmed_model_calls
                .map_or_else(|| "unavailable".to_owned(), |calls| calls.to_string()),
            polling.prompt_tokens,
            polling.completion_tokens,
            Duration::from_nanos(polling.model_duration_ns).as_secs_f64(),
        );
    }
    if let Some(error) = &arm.operational_error {
        let _ = writeln!(output, "{name} runner error: {error}");
    }
    if let Some(error) = &arm.event_error {
        let _ = writeln!(output, "{name} event error: {error}");
    }
    if let Some(error) = &arm.trajectory_error {
        let _ = writeln!(output, "{name} trajectory error: {error}");
    }
}

#[cfg(all(test, unix))]
#[path = "differential/tests.rs"]
mod tests;
