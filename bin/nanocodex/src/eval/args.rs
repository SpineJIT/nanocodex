use std::path::PathBuf;

use clap::Args;

pub(crate) const DEFAULT_TRIALS: u16 = 5;
pub(crate) const DEFAULT_HOST_UTILIZATION_PERCENT: u8 = 80;

/// Shared host-wide scheduling policy for ordinary and differential evals.
#[derive(Clone, Debug, Args)]
pub(crate) struct SchedulingArgs {
    /// Number of independent trials per task and configuration.
    #[arg(
        long,
        default_value_t = DEFAULT_TRIALS,
        value_parser = clap::value_parser!(u16).range(1..)
    )]
    pub(crate) trials: u16,

    /// Maximum number of evaluation work items active at once.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub(crate) concurrency: Option<u16>,

    /// Target ceiling on admitted host memory.
    #[arg(long, value_name = "MIB", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) max_memory_mb: Option<u64>,

    /// Percentage of detected host CPU and memory used for omitted limits.
    #[arg(
        long,
        default_value_t = DEFAULT_HOST_UTILIZATION_PERCENT,
        value_name = "PERCENT",
        value_parser = clap::value_parser!(u8).range(1..=100)
    )]
    pub(crate) host_utilization: u8,
}

/// Shared guest-runtime and image-cache inputs.
#[derive(Clone, Debug, Args)]
pub(crate) struct VmPreparationArgs {
    /// Use this prebuilt guest-runtime ELF instead of the workspace build.
    #[arg(long, value_name = "ELF")]
    pub(crate) vm_guest_runtime: Option<PathBuf>,

    /// Content-addressed VM cache shared across evaluation jobs.
    #[arg(long, value_name = "DIRECTORY", default_value = ".cache/vm")]
    pub(crate) vm_cache: PathBuf,

    /// Refresh task images instead of reusing their local resolution.
    #[arg(long)]
    pub(crate) vm_refresh: bool,
}
