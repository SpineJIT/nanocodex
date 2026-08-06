use std::path::PathBuf;

use clap::Args;

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
