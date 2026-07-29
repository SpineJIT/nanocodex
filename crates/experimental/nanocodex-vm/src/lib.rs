#![cfg_attr(feature = "host", doc = include_str!("../README.md"))]
#![cfg_attr(not(feature = "host"), doc = include_str!("../GUEST_RUNTIME.md"))]
#![deny(unsafe_code, missing_docs, rustdoc::broken_intra_doc_links)]

#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod capabilities;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod command;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod config;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod egress;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod gvproxy;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub mod image;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod krun;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod process;
pub mod tools;
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
mod workspace;

/// Low-level host-side VM configuration and lifecycle components.
///
/// Most applications should start with [`crate::VmWorkspaceBuilder`]. This
/// module is for custom VMM entry points, network/egress policy, and direct
/// libkrun lifecycle ownership.
#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub mod host {
    pub use crate::{
        capabilities::{Capabilities, KrunFeature},
        command::GuestCommand,
        config::{BlockDevice, Network, RootFilesystem, SharedDirectory, VmConfig},
        egress::{
            EgressError, EgressFile, EgressLease, EgressMount, GUEST_EGRESS_ROOT,
            MAX_EGRESS_FILE_BYTES,
        },
        gvproxy::{Gvproxy, GvproxyError},
        krun::{KrunVm, KrunVmControl, VmError},
        process::{PrivateVmProcessConfig, VmProcessConfig, VmProcessError},
    };
}

#[cfg(all(feature = "host", any(target_os = "linux", target_os = "macos")))]
pub use workspace::{VmWorkspace, VmWorkspaceBuilder, VmWorkspaceError};
