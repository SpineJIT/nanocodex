use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use chrono::{DateTime, Utc};
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
    evaluator::{AttemptAgent, EvalAttempt},
    job::{create_durable_directory_all, sync_directory},
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
const PROGRESS_FILE: &str = "progress.jsonl";
const PROGRESS_SCHEMA_VERSION: u32 = 1;
const PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const PROGRESS_HEARTBEAT_SUMMARY_CHARS: usize = 64;
const PROGRESS_SUMMARY_CHARS: usize = 180;
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
    memory: Mutex<DifferentialMemoryPlanner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DifferentialProfile {
    thinking: Thinking,
    nanocodex_tool_mode: NanocodexToolMode,
    codex_tool_mode: CodexToolMode,
}

impl DifferentialProfile {
    const fn new(
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
struct DifferentialMemoryPlan {
    guest_memory_mb: u64,
    nanocodex_admission_memory_mb: u64,
    codex_admission_memory_mb: u64,
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

#[derive(Serialize)]
struct DifferentialSchedule {
    declared_pair_memory_mb: u64,
    configured_guest_memory_mb: u64,
    nanocodex_admission_memory_mb: u64,
    codex_admission_memory_mb: u64,
    memory_attempt: usize,
}

const fn differential_pair_memory_mb(arm_memory_mb: u64) -> u64 {
    arm_memory_mb.saturating_mul(2)
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
            initial_guest_memory_mb: DEFAULT_DIFFERENTIAL_GUEST_MEMORY_MB,
            memory_profile_path: None,
        }
    }

    /// Runs one independent matched pair.
    ///
    /// # Errors
    ///
    /// Returns an error when the comparison cannot be prepared or retained.
    /// Runs one independent matched pair.
    ///
    /// # Errors
    ///
    /// Returns an error when the comparison cannot be prepared or retained.
    pub async fn task(&self, task: Task) -> DifferentialResult<DifferentialReport> {
        let profile = DifferentialProfile::new(
            self.inner.thinking,
            self.inner.nanocodex_tool_mode,
            self.inner.codex_tool_mode,
        );
        let memory_plan = self.memory_plan(&task);
        let result = DifferentialComparison {
            trial: 1,
            schedule: DifferentialSchedule {
                declared_pair_memory_mb: differential_pair_memory_mb(task.resources().memory_mb),
                configured_guest_memory_mb: memory_plan.guest_memory_mb,
                nanocodex_admission_memory_mb: memory_plan.nanocodex_admission_memory_mb,
                codex_admission_memory_mb: memory_plan.codex_admission_memory_mb,
                memory_attempt: 1,
            },
            task,
            nanocodex: self.inner.nanocodex.clone(),
            codex_sha256: self.inner.codex_sha256.clone(),
            codex_release: Arc::clone(&self.inner.codex_release),
            codex_auth: self.inner.codex_auth.clone(),
            vm: Arc::clone(&self.inner.vm),
            output: self.inner.output.clone(),
            thinking: profile.thinking,
            web_search: self.inner.web_search,
            nanocodex_tool_mode: profile.nanocodex_tool_mode,
            codex_tool_mode: profile.codex_tool_mode,
            nanocodex_build: self.inner.nanocodex_build.clone(),
            memory_plan,
        }
        .run()
        .await;
        if let Ok(report) = &result
            && let Err(error) = self
                .inner
                .memory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .observe(report)
        {
            warn!(
                task = report.task_name(),
                error = %error,
                "failed to persist differential memory observation"
            );
        }
        result
    }

    fn memory_plan(&self, task: &Task) -> DifferentialMemoryPlan {
        self.inner
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .plan(task, None)
    }
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
        create_durable_comparison_directory(&output, &comparison_directory)?;
        let progress_path = comparison_directory.join(PROGRESS_FILE);
        let (progress, progress_recorder) =
            DiffProgress::start(progress_path.clone(), started).await?;
        let profile = DifferentialProfile::new(thinking, nanocodex_tool_mode, codex_tool_mode);
        progress.emit_comparison_started(
            &task,
            profile,
            trial,
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
        let (mut nanocodex_arm, mut codex_arm) = tokio::join!(
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
        );
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

    /// Sets the low per-arm guest allocation used until a task has measured
    /// memory history. The allocation is always capped by the task declaration.
    #[must_use]
    pub const fn initial_guest_memory_mb(mut self, memory_mb: u64) -> Self {
        self.initial_guest_memory_mb = memory_mb;
        self
    }

    /// Selects the durable task-memory profile shared by future coordinates.
    #[must_use]
    pub fn memory_profile_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.memory_profile_path = Some(path.into());
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
                memory: Mutex::new(memory),
            }),
        })
    }
}

impl DifferentialReport {
    /// Returns the matched verifier classification.
    #[must_use]
    pub const fn classification(&self) -> DifferentialClassification {
        self.classification
    }

    fn task_name(&self) -> &str {
        &self.task.name
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

    const fn configured_guest_memory_mb(&self) -> u64 {
        self.schedule.configured_guest_memory_mb
    }

    const fn declared_arm_memory_mb(&self) -> u64 {
        self.schedule.declared_pair_memory_mb / 2
    }

    fn is_memory_calibration_success(&self) -> bool {
        !self.oom_detected()
            && [&self.nanocodex, &self.codex].into_iter().all(|arm| {
                arm.outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.outcome() != EvalOutcome::InfrastructureError)
            })
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
        let attempt_directory = &outcome.artifacts().directory;
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
    let task = outcome.task();
    let trajectory = match projection {
        TrajectoryProjection::Nanocodex => {
            if let Some(error) = recording.atif_error {
                return Err(diff_error!(error));
            }
            match outcome.agent() {
                Some(agent) => recording.atif.finish(task, agent),
                None => recording.atif.finish_failure(task),
            }
        }
        TrajectoryProjection::Codex { version } => {
            let agent = outcome.agent().ok_or_else(|| {
                diff_error!("stock Codex attempt retained no terminal agent result")
            })?;
            let events = outcome
                .artifacts()
                .directory
                .join("agent/codex-events.jsonl");
            let version = version.resolve()?;
            project_codex_atif(&events, task.prompt(), agent, &version).wrap_err_with(|| {
                format!("failed to project stock Codex stream {}", events.display())
            })?
        }
    };
    let path = outcome.artifacts().directory.join(TRAJECTORY_FILE);
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

fn prepare_output_parent(output: &Path) -> InternalResult<PathBuf> {
    create_durable_directory_all(output).wrap_err_with(|| {
        format!(
            "failed to durably create output directory {}",
            output.display()
        )
    })
}

fn create_durable_comparison_directory(output: &Path, directory: &Path) -> InternalResult<()> {
    create_durable_comparison_directory_with_sync(output, directory, sync_directory)
}

fn create_durable_comparison_directory_with_sync<F>(
    output: &Path,
    directory: &Path,
    mut sync: F,
) -> InternalResult<()>
where
    F: FnMut(&Path) -> io::Result<()>,
{
    fs::create_dir(directory).wrap_err_with(|| {
        format!(
            "failed to create comparison directory {}",
            directory.display()
        )
    })?;
    sync(output).wrap_err_with(|| {
        format!(
            "failed to durably publish comparison directory {}",
            directory.display()
        )
    })
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
    write_json_atomic_with_sync(path, value, sync_directory)
}

fn write_json_atomic_with_sync<F>(
    path: &Path,
    value: &impl Serialize,
    mut sync: F,
) -> InternalResult<()>
where
    F: FnMut(&Path) -> io::Result<()>,
{
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
    sync(parent).wrap_err_with(|| format!("failed to durably publish {}", path.display()))?;
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "differential/tests.rs"]
mod tests;
