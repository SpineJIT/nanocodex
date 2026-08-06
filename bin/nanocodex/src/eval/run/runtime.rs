use std::{
    fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    time::{Instant, UNIX_EPOCH},
};

use eyre::{Result, eyre};
use nanocodex_vm::tools::{GuestRuntimeDisk, GuestRuntimeDiskStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::process::Command;
use tracing::{info, warn};

use super::write_json_atomic;

const DEFAULT_VM_CACHE: &str = ".cache/vm";
#[cfg(target_arch = "aarch64")]
const VM_GUEST_TARGET: &str = "aarch64-unknown-linux-musl";
#[cfg(target_arch = "x86_64")]
const VM_GUEST_TARGET: &str = "x86_64-unknown-linux-musl";
#[cfg(target_arch = "aarch64")]
const VM_GUEST_ELF_MACHINE: u16 = 183;
#[cfg(target_arch = "x86_64")]
const VM_GUEST_ELF_MACHINE: u16 = 62;
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
compile_error!("Evaluator VM guests are only supported on aarch64 and x86_64 hosts");
const VM_GUEST_BUILD_RECORD_VERSION: u32 = 1;

pub(crate) async fn prepare_vm_guest_runtime() -> Result<PathBuf> {
    prepare_vm_guest_runtime_from(None, Path::new(DEFAULT_VM_CACHE)).await
}

pub(crate) async fn prepare_vm_guest_runtime_from(
    prebuilt: Option<&Path>,
    cache: &Path,
) -> Result<PathBuf> {
    let started_at = Instant::now();
    let environment_prebuilt = std::env::var_os("NANOCODEX_VM_GUEST_RUNTIME").map(PathBuf::from);
    let prebuilt = prebuilt.or(environment_prebuilt.as_deref());
    let source = resolve_vm_guest_runtime_source(prebuilt).await?;
    let (bytes, _) = stable_file_bytes(&source.path)?;
    validate_vm_guest_elf(&bytes, &source.path)?;
    let runtime_disk = GuestRuntimeDisk::prepare(&source.path, cache)?;
    record_guest_runtime_ready(
        started_at,
        source.build_status,
        source.source,
        &runtime_disk,
    );
    Ok(runtime_disk.path().to_path_buf())
}

struct SourceGuestRuntime {
    path: PathBuf,
    build_status: &'static str,
    source: &'static str,
}

async fn resolve_vm_guest_runtime_source(prebuilt: Option<&Path>) -> Result<SourceGuestRuntime> {
    if let Some(prebuilt) = prebuilt {
        let runtime = fs::canonicalize(prebuilt).map_err(|error| {
            eyre!(
                "failed to resolve prebuilt VM guest runtime {}: {error}",
                prebuilt.display()
            )
        })?;
        if !runtime.is_file() {
            return Err(eyre!(
                "prebuilt VM guest runtime is not a file: {}",
                runtime.display()
            ));
        }
        return Ok(SourceGuestRuntime {
            path: runtime,
            build_status: "prebuilt",
            source: "explicit_binary",
        });
    }

    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| eyre!("nanocodex binary crate is not inside its Cargo workspace"))?;
    validate_vm_guest_source_identity(workspace).await?;
    let runtime = workspace
        .join("target")
        .join(VM_GUEST_TARGET)
        .join("debug/nanocodex-vm-guest");
    let build_status = if vm_guest_runtime_is_fresh(workspace, &runtime)? {
        "hit"
    } else {
        let previous_runtime = file_metadata_snapshot(&runtime)?;
        let exit = vm_guest_build_command(workspace).status().await?;
        if !exit.success() {
            return Err(eyre!("building the VM guest runtime failed with {exit}"));
        }
        let current_runtime = file_metadata_snapshot(&runtime)?;
        let build_status = if previous_runtime.is_some() && previous_runtime == current_runtime {
            "indexed"
        } else {
            "rebuilt"
        };
        write_vm_guest_build_record(workspace, &runtime)?;
        build_status
    };
    if !runtime.is_file() {
        return Err(eyre!(
            "Cargo completed without producing {}",
            runtime.display()
        ));
    }
    Ok(SourceGuestRuntime {
        path: fs::canonicalize(runtime)?,
        build_status,
        source: "host_commit_source",
    })
}

fn validate_vm_guest_elf(bytes: &[u8], path: &Path) -> Result<()> {
    let header = bytes.get(..20).ok_or_else(|| {
        eyre!(
            "VM guest runtime is too short to contain an ELF header: {}",
            path.display()
        )
    })?;
    if &header[..4] != b"\x7fELF" {
        return Err(eyre!(
            "VM guest runtime is not an ELF executable: {}",
            path.display()
        ));
    }
    let class = header[4];
    let byte_order = header[5];
    let machine = u16::from_le_bytes([header[18], header[19]]);
    if class != 2 || byte_order != 1 || machine != VM_GUEST_ELF_MACHINE {
        return Err(eyre!(
            "VM guest runtime {} has ELF class {class}, byte order {byte_order}, and e_machine \
             {machine}; target {VM_GUEST_TARGET} requires 64-bit little-endian e_machine \
             {VM_GUEST_ELF_MACHINE}",
            path.display()
        ));
    }
    Ok(())
}

fn stable_file_bytes(path: &Path) -> Result<(Vec<u8>, FileMetadataSnapshot)> {
    let snapshot = file_metadata_snapshot(path)?
        .ok_or_else(|| eyre!("identity input is not a regular file: {}", path.display()))?;
    let mut bytes = Vec::with_capacity(usize::try_from(snapshot.bytes).unwrap_or(0));
    fs::File::open(path)?.read_to_end(&mut bytes)?;
    if file_metadata_snapshot(path)? != Some(snapshot) {
        return Err(eyre!(
            "identity input changed while it was being read: {}",
            path.display()
        ));
    }
    Ok((bytes, snapshot))
}

fn record_guest_runtime_ready(
    started_at: Instant,
    build_status: &str,
    source: &str,
    runtime_disk: &GuestRuntimeDisk,
) {
    let cache_status = match runtime_disk.status() {
        GuestRuntimeDiskStatus::Hit => "hit",
        GuestRuntimeDiskStatus::Created => "created",
    };
    info!(
        target: "nanocodex_vm",
        duration_ns = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX),
        vm_guest_build_status = build_status,
        vm_guest_target = VM_GUEST_TARGET,
        vm_guest_runtime_source = source,
        vm_guest_runtime_cache_status = cache_status,
        vm_guest_runtime_digest = runtime_disk.digest(),
        vm_guest_runtime_disk = %runtime_disk.path().display(),
        "VM guest runtime ready"
    );
}

const VM_GUEST_SOURCE_PATHS: [&str; 10] = [
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
    "crates/nanocodex-oai-api",
    "crates/nanocodex-tools",
    "crates/experimental/nanocodex-vm",
    "scripts/aarch64-unknown-linux-musl-linker",
    "scripts/aarch64-unknown-linux-musl-ar",
    "scripts/x86_64-unknown-linux-musl-linker",
    "scripts/x86_64-unknown-linux-musl-ar",
];

async fn validate_vm_guest_source_identity(workspace: &Path) -> Result<()> {
    let head = Command::new("git")
        .current_dir(workspace)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .await?;
    if !head.status.success() {
        return Err(eyre!(
            "cannot bind VM guest source to host commit {}; pass \
             --vm-guest-runtime with a pinned prebuilt ELF",
            env!("VERGEN_GIT_SHA")
        ));
    }
    let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    if head != env!("VERGEN_GIT_SHA") {
        return Err(eyre!(
            "refusing to build the VM guest runtime from source commit {head}; host binary was \
             built from {}. Pass --vm-guest-runtime with a pinned prebuilt ELF",
            env!("VERGEN_GIT_SHA")
        ));
    }

    let status = Command::new("git")
        .current_dir(workspace)
        .arg("status")
        .arg("--porcelain=v1")
        .arg("--untracked-files=all")
        .arg("--")
        .args(VM_GUEST_SOURCE_PATHS)
        .output()
        .await?;
    if !status.status.success() {
        return Err(eyre!(
            "cannot inspect VM guest source at {}; pass --vm-guest-runtime with a pinned prebuilt ELF",
            workspace.display()
        ));
    }
    let dirty = String::from_utf8_lossy(&status.stdout);
    if !dirty.trim().is_empty() {
        return Err(eyre!(
            "refusing to build the VM guest runtime from dirty source: {}; pass \
             --vm-guest-runtime with a pinned prebuilt ELF",
            dirty.lines().take(8).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}

fn vm_guest_build_command(workspace: &Path) -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(workspace)
        .arg("build")
        .arg("--quiet")
        .arg("--locked")
        .arg("--target")
        .arg(VM_GUEST_TARGET)
        .arg("--package")
        .arg("nanocodex-vm")
        .arg("--bin")
        .arg("nanocodex-vm-guest")
        .arg("--no-default-features")
        .arg("--features")
        .arg("guest-runtime");
    command
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct VmGuestBuildRecord {
    version: u32,
    target: String,
    runtime_bytes: u64,
    runtime_modified_unix_ns: u64,
    input_count: usize,
    input_metadata_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileMetadataSnapshot {
    bytes: u64,
    modified_unix_ns: u64,
}

fn vm_guest_runtime_is_fresh(workspace: &Path, runtime: &Path) -> Result<bool> {
    let path = vm_guest_build_record_path(workspace);
    let record = match fs::read(&path) {
        Ok(contents) => match serde_json::from_slice::<VmGuestBuildRecord>(&contents) {
            Ok(record) => record,
            Err(error) => {
                warn!(
                    target: "nanocodex_eval",
                    cache_record = %path.display(),
                    %error,
                    "ignoring invalid VM guest build record"
                );
                return Ok(false);
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(vm_guest_build_record(workspace, runtime)?.as_ref() == Some(&record))
}

fn write_vm_guest_build_record(workspace: &Path, runtime: &Path) -> Result<()> {
    let record = vm_guest_build_record(workspace, runtime)?.ok_or_else(|| {
        eyre!(
            "Cargo completed without producing {} and its dependency record",
            runtime.display()
        )
    })?;
    write_json_atomic(&vm_guest_build_record_path(workspace), &record)
}

fn vm_guest_build_record(workspace: &Path, runtime: &Path) -> Result<Option<VmGuestBuildRecord>> {
    let Some(runtime_metadata) = file_metadata_snapshot(runtime)? else {
        return Ok(None);
    };
    let dependency_path = runtime.with_extension("d");
    let dependencies = match fs::read_to_string(&dependency_path) {
        Ok(contents) => parse_cargo_dep_info(&contents)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut inputs = dependencies;
    inputs.extend([
        workspace.join("Cargo.toml"),
        workspace.join("Cargo.lock"),
        workspace.join(".cargo/config.toml"),
        workspace.join("crates/nanocodex-oai-api/Cargo.toml"),
        workspace.join("crates/nanocodex-tools/Cargo.toml"),
        workspace.join("crates/experimental/nanocodex-vm/Cargo.toml"),
    ]);
    for script in [
        format!("scripts/{VM_GUEST_TARGET}-linker"),
        format!("scripts/{VM_GUEST_TARGET}-ar"),
    ] {
        let path = workspace.join(script);
        if path.exists() {
            inputs.push(path);
        }
    }
    inputs.sort_unstable();
    inputs.dedup();

    let mut digest = Sha256::new();
    digest.update(b"nanocodex-vm-guest-build-inputs-v1\0");
    for input in &inputs {
        let Some(metadata) = file_metadata_snapshot(input)? else {
            return Ok(None);
        };
        digest.update(input.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(metadata.bytes.to_le_bytes());
        digest.update(metadata.modified_unix_ns.to_le_bytes());
    }
    Ok(Some(VmGuestBuildRecord {
        version: VM_GUEST_BUILD_RECORD_VERSION,
        target: VM_GUEST_TARGET.to_owned(),
        runtime_bytes: runtime_metadata.bytes,
        runtime_modified_unix_ns: runtime_metadata.modified_unix_ns,
        input_count: inputs.len(),
        input_metadata_digest: hex::encode(digest.finalize()),
    }))
}

fn vm_guest_build_record_path(workspace: &Path) -> PathBuf {
    workspace
        .join(DEFAULT_VM_CACHE)
        .join("runtime-build-records")
        .join(format!("{VM_GUEST_TARGET}.json"))
}

fn file_metadata_snapshot(path: &Path) -> io::Result<Option<FileMetadataSnapshot>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    Ok(Some(FileMetadataSnapshot {
        bytes: metadata.len(),
        modified_unix_ns: u64::try_from(modified.as_nanos()).map_err(io::Error::other)?,
    }))
}

fn parse_cargo_dep_info(contents: &str) -> io::Result<Vec<PathBuf>> {
    let (_, dependencies) = contents
        .split_once(": ")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid Cargo dep-info"))?;
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in dependencies.chars() {
        if escaped {
            if character != '\n' && character != '\r' {
                current.push(character);
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character.is_whitespace() {
            if !current.is_empty() {
                paths.push(PathBuf::from(std::mem::take(&mut current)));
            }
        } else {
            current.push(character);
        }
    }
    if escaped {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Cargo dep-info ends with an escape",
        ));
    }
    if !current.is_empty() {
        paths.push(PathBuf::from(current));
    }
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Cargo dep-info contains no dependencies",
        ));
    }
    Ok(paths)
}
