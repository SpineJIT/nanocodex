use super::*;

#[derive(Debug)]
pub(super) struct PreparedGuestRuntime {
    pub(super) disk: PathBuf,
    pub(super) identity: Option<RetainedGuestRuntime>,
}

pub(super) async fn prepare_runtime_for_vm(
    rootfs: Option<&Path>,
    guest_runtime: Option<&Path>,
    job: &Path,
    origin: Option<&RetainedGuestRuntimeOrigin>,
) -> Result<PreparedGuestRuntime> {
    let embedded_runtime = rootfs
        .filter(|rootfs| rootfs.is_dir())
        .map(|rootfs| rootfs.join(EMBEDDED_GUEST_TOOL_RUNTIME.trim_start_matches('/')));
    if let Some(rootfs) = rootfs.filter(|rootfs| rootfs.is_dir())
        && guest_runtime.is_some()
    {
        return Err(eyre!(
            "--vm-guest-runtime cannot override the runtime embedded in directory rootfs {}",
            rootfs.display()
        ));
    }
    let block_runtime = embedded_runtime.is_none();
    if let Some(origin) = origin {
        return prepare_retained_guest_runtime(job, origin, guest_runtime, block_runtime);
    }

    let source = match embedded_runtime {
        Some(runtime) => SourceGuestRuntime {
            path: fs::canonicalize(&runtime).map_err(|error| {
                eyre!(
                    "failed to resolve VM guest runtime embedded in {}: {error}",
                    runtime.display()
                )
            })?,
            build_status: "embedded",
            source: "embedded_rootfs",
        },
        None => resolve_vm_guest_runtime_source(guest_runtime).await?,
    };
    prepare_new_guest_runtime(job, source, block_runtime)
}

const EMBEDDED_GUEST_TOOL_RUNTIME: &str = "/usr/local/bin/nanocodex-vm-guest";
const GUEST_RUNTIME_DISK_BINARY_PATH: &str = "/nanocodex-vm-guest";
pub(super) const GUEST_RUNTIME_ARTIFACT_ROOT: &str = "guest-runtime/artifacts";
pub(super) const GUEST_RUNTIME_CACHE_ROOT: &str = "guest-runtime/cache";
const DEFAULT_VM_CACHE: &str = ".cache/vm";
#[cfg(target_arch = "aarch64")]
pub(super) const VM_GUEST_TARGET: &str = "aarch64-unknown-linux-musl";
#[cfg(target_arch = "x86_64")]
pub(super) const VM_GUEST_TARGET: &str = "x86_64-unknown-linux-musl";
#[cfg(target_arch = "aarch64")]
pub(super) const VM_GUEST_ELF_MACHINE: u16 = 183;
#[cfg(target_arch = "x86_64")]
pub(super) const VM_GUEST_ELF_MACHINE: u16 = 62;
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

fn prepare_new_guest_runtime(
    job: &Path,
    source: SourceGuestRuntime,
    block_runtime: bool,
) -> Result<PreparedGuestRuntime> {
    let started_at = Instant::now();
    let (bytes, _) = stable_file_bytes(&source.path)?;
    validate_vm_guest_elf(&bytes, &source.path)?;
    let (artifact_path, artifact) = retain_guest_runtime_bytes(job, &bytes)?;
    let (disk, runtime_disk_digest, cache_status) = if block_runtime {
        let runtime_disk = prepare_job_guest_runtime_disk(job, &artifact)?;
        let cache_status = runtime_disk.status();
        (
            runtime_disk.path().to_path_buf(),
            Some(runtime_disk.digest().to_owned()),
            Some(cache_status),
        )
    } else {
        (artifact, None, None)
    };
    let binary_sha256 = hex::encode(Sha256::digest(&bytes));
    if let Some(cache_status) = cache_status {
        let runtime_disk = GuestRuntimeDiskView {
            path: &disk,
            digest: runtime_disk_digest.as_deref().unwrap_or_default(),
            status: cache_status,
        };
        record_guest_runtime_view(started_at, source.build_status, source.source, runtime_disk);
    }
    Ok(PreparedGuestRuntime {
        disk,
        identity: Some(RetainedGuestRuntime {
            target: VM_GUEST_TARGET.to_owned(),
            binary_sha256,
            runtime_disk_digest,
            artifact_path: Some(artifact_path),
            source: source.source.to_owned(),
            source_path: source.path,
            host_git_sha: env!("VERGEN_GIT_SHA").to_owned(),
        }),
    })
}

pub(super) fn prepare_retained_guest_runtime(
    job: &Path,
    origin: &RetainedGuestRuntimeOrigin,
    requested: Option<&Path>,
    block_runtime: bool,
) -> Result<PreparedGuestRuntime> {
    if origin.runtime.target != VM_GUEST_TARGET {
        return Err(eyre!(
            "retained VM guest runtime targets {}, but this host requires {}",
            origin.runtime.target,
            VM_GUEST_TARGET
        ));
    }
    let bytes = retained_guest_runtime_bytes(origin, requested)?;
    validate_vm_guest_elf(&bytes, Path::new("<retained VM guest runtime>"))?;
    let binary_sha256 = hex::encode(Sha256::digest(&bytes));
    if binary_sha256 != origin.runtime.binary_sha256 {
        return Err(eyre!(
            "retained VM guest runtime bytes hash to {binary_sha256}, expected {}",
            origin.runtime.binary_sha256
        ));
    }
    let (artifact_path, artifact) = retain_guest_runtime_bytes(job, &bytes)?;
    let (disk, runtime_disk_digest) = if block_runtime {
        let expected = origin
            .runtime
            .runtime_disk_digest
            .as_deref()
            .ok_or_else(|| {
                eyre!("retained block VM guest runtime is missing its runtime disk digest")
            })?;
        let runtime_disk = prepare_job_guest_runtime_disk(job, &artifact)?;
        if runtime_disk.digest() != expected {
            return Err(eyre!(
                "retained VM guest runtime disk digest is {}, expected {expected}",
                runtime_disk.digest()
            ));
        }
        record_guest_runtime_ready(Instant::now(), "retained", "job_artifact", &runtime_disk);
        (
            runtime_disk.path().to_path_buf(),
            Some(runtime_disk.digest().to_owned()),
        )
    } else {
        if origin.runtime.runtime_disk_digest.is_some() {
            return Err(eyre!(
                "retained directory-rootfs guest runtime unexpectedly has a disk digest"
            ));
        }
        (artifact, None)
    };
    let mut identity = origin.runtime.clone();
    identity.artifact_path = Some(artifact_path);
    identity.runtime_disk_digest = runtime_disk_digest;
    Ok(PreparedGuestRuntime {
        disk,
        identity: Some(identity),
    })
}

fn retained_guest_runtime_bytes(
    origin: &RetainedGuestRuntimeOrigin,
    requested: Option<&Path>,
) -> Result<Vec<u8>> {
    let requested_bytes = if let Some(requested) = requested {
        let requested = fs::canonicalize(requested)?;
        let (bytes, _) = stable_file_bytes(&requested)?;
        validate_vm_guest_elf(&bytes, &requested)?;
        let digest = hex::encode(Sha256::digest(&bytes));
        if digest != origin.runtime.binary_sha256 {
            return Err(eyre!(
                "requested VM guest runtime {} hashes to {digest}, but the retained workload \
                 requires {}",
                requested.display(),
                origin.runtime.binary_sha256
            ));
        }
        Some(bytes)
    } else {
        None
    };
    if let Some(artifact_path) = &origin.runtime.artifact_path {
        let expected = guest_runtime_artifact_path(&origin.runtime.binary_sha256)?;
        if artifact_path != &expected {
            return Err(eyre!(
                "retained VM guest runtime artifact path {} does not match its content address {}",
                artifact_path.display(),
                expected.display()
            ));
        }
        let artifact = origin.job.join(artifact_path);
        let artifact_parent = artifact
            .parent()
            .ok_or_else(|| eyre!("VM guest runtime artifact path has no parent"))?;
        ensure_job_owned_path(&origin.job, artifact_parent)?;
        match fs::symlink_metadata(&artifact) {
            Ok(metadata) if metadata.file_type().is_file() => {
                let (bytes, _) = stable_file_bytes(&artifact)?;
                return Ok(bytes);
            }
            Ok(_) => {
                return Err(eyre!(
                    "retained VM guest runtime artifact is not a regular job-owned file: {}",
                    artifact.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        return requested_bytes.map_or_else(|| recover_retained_guest_runtime_disk(origin), Ok);
    }
    if let Some(bytes) = requested_bytes {
        return Ok(bytes);
    }
    recover_retained_guest_runtime_disk(origin)
}

fn recover_retained_guest_runtime_disk(origin: &RetainedGuestRuntimeOrigin) -> Result<Vec<u8>> {
    let digest = origin
        .runtime
        .runtime_disk_digest
        .as_deref()
        .ok_or_else(|| {
            eyre!(
                "retained VM guest runtime has no immutable artifact; pass --vm-guest-runtime with \
             the exact ELF or start a new job with --new"
            )
        })?;
    validate_sha256_digest(digest, "runtime disk digest")?;
    let disks = [
        origin
            .job
            .join(GUEST_RUNTIME_CACHE_ROOT)
            .join("runtimes")
            .join(digest)
            .join("runtime.ext4"),
        Path::new(DEFAULT_VM_CACHE)
            .join("runtimes")
            .join(digest)
            .join("runtime.ext4"),
    ];
    for disk in disks {
        let Ok(mut reader) = Reader::new(&disk) else {
            continue;
        };
        if let Ok(bytes) = reader.read_file(GUEST_RUNTIME_DISK_BINARY_PATH, 0, None) {
            return Ok(bytes);
        }
    }
    Err(eyre!(
        "retained VM guest runtime artifact and runtime disk {digest} are unavailable; pass \
         --vm-guest-runtime with the exact ELF or start a new job with --new"
    ))
}

pub(super) fn prepare_job_guest_runtime_disk(
    job: &Path,
    artifact: &Path,
) -> Result<GuestRuntimeDisk> {
    let cache = job.join(GUEST_RUNTIME_CACHE_ROOT);
    ensure_job_owned_path(job, &cache)?;
    let runtime_disk = GuestRuntimeDisk::prepare(artifact, &cache)?;
    ensure_job_owned_path(job, &cache)?;
    Ok(runtime_disk)
}

pub(super) fn retain_guest_runtime_bytes(job: &Path, bytes: &[u8]) -> Result<(PathBuf, PathBuf)> {
    validate_vm_guest_elf(bytes, Path::new("<VM guest runtime artifact>"))?;
    let digest = hex::encode(Sha256::digest(bytes));
    let relative = guest_runtime_artifact_path(&digest)?;
    let artifact = job.join(&relative);
    let parent = artifact
        .parent()
        .ok_or_else(|| eyre!("VM guest runtime artifact path has no parent"))?;
    ensure_job_owned_path(job, parent)?;
    fs::create_dir_all(parent)?;
    ensure_job_owned_path(job, parent)?;
    match fs::symlink_metadata(&artifact) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let (retained, _) = stable_file_bytes(&artifact)?;
            if retained != bytes {
                return Err(eyre!(
                    "content-addressed VM guest runtime artifact has conflicting bytes: {}",
                    artifact.display()
                ));
            }
            return Ok((relative, artifact));
        }
        Ok(_) => {
            return Err(eyre!(
                "content-addressed VM guest runtime artifact is not a regular file: {}",
                artifact.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o755))?;
    match temporary.persist_noclobber(&artifact) {
        Ok(file) => file.sync_all()?,
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            let (retained, _) = stable_file_bytes(&artifact)?;
            if retained != bytes {
                return Err(eyre!(
                    "content-addressed VM guest runtime artifact has conflicting bytes: {}",
                    artifact.display()
                ));
            }
        }
        Err(error) => return Err(error.error.into()),
    }
    fs::File::open(parent)?.sync_all()?;
    Ok((relative, artifact))
}

// Eval job directories are application-owned. This rejects pre-existing path and symlink escapes
// and rechecks created paths, but does not claim a capability boundary against hostile concurrent
// mutation of the job tree.
pub(super) fn ensure_job_owned_path(job: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(job).map_err(|_| {
        eyre!(
            "VM guest runtime path {} escapes job {}",
            path.display(),
            job.display()
        )
    })?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(eyre!(
            "VM guest runtime path {} escapes job {}",
            path.display(),
            job.display()
        ));
    }
    let job = fs::canonicalize(job)?;
    let mut existing = path;
    let resolved = loop {
        match fs::symlink_metadata(existing) {
            Ok(_) => break fs::canonicalize(existing)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or_else(|| {
                    eyre!(
                        "VM guest runtime path has no existing ancestor: {}",
                        path.display()
                    )
                })?;
            }
            Err(error) => return Err(error.into()),
        }
    };
    if !resolved.starts_with(&job) {
        return Err(eyre!(
            "VM guest runtime path {} escapes job {}",
            resolved.display(),
            job.display()
        ));
    }
    Ok(())
}

fn guest_runtime_artifact_path(binary_sha256: &str) -> Result<PathBuf> {
    validate_sha256_digest(binary_sha256, "guest runtime binary digest")?;
    Ok(Path::new(GUEST_RUNTIME_ARTIFACT_ROOT)
        .join(binary_sha256)
        .join("nanocodex-vm-guest"))
}

fn validate_sha256_digest(digest: &str, label: &str) -> Result<()> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(eyre!("{label} is not a lowercase SHA-256 digest: {digest}"))
    }
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

pub(super) fn stable_file_sha256(path: &Path) -> Result<String> {
    let (bytes, _) = stable_file_bytes(path)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn record_guest_runtime_ready(
    started_at: Instant,
    build_status: &str,
    source: &str,
    runtime_disk: &GuestRuntimeDisk,
) {
    record_guest_runtime_view(
        started_at,
        build_status,
        source,
        GuestRuntimeDiskView {
            path: runtime_disk.path(),
            digest: runtime_disk.digest(),
            status: runtime_disk.status(),
        },
    );
}

struct GuestRuntimeDiskView<'a> {
    path: &'a Path,
    digest: &'a str,
    status: GuestRuntimeDiskStatus,
}

fn record_guest_runtime_view(
    started_at: Instant,
    build_status: &str,
    source: &str,
    runtime_disk: GuestRuntimeDiskView<'_>,
) {
    let cache_status = match runtime_disk.status {
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
        vm_guest_runtime_digest = runtime_disk.digest,
        vm_guest_runtime_disk = %runtime_disk.path.display(),
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
    validate_vm_guest_commit(env!("VERGEN_GIT_SHA"), &head)?;

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
            "cannot inspect VM guest source at {}; pass --vm-guest-runtime \
             with a pinned prebuilt ELF",
            workspace.display()
        ));
    }
    let dirty = String::from_utf8_lossy(&status.stdout);
    if !dirty.trim().is_empty() {
        return Err(eyre!(
            "refusing to build the VM guest runtime from source that differs from host commit {}: \
             {}; pass --vm-guest-runtime with a pinned prebuilt ELF",
            env!("VERGEN_GIT_SHA"),
            dirty.lines().take(8).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}

pub(super) fn validate_vm_guest_commit(host: &str, source: &str) -> Result<()> {
    if host == source {
        return Ok(());
    }
    Err(eyre!(
        "refusing to build the VM guest runtime from source commit {source}; \
         host binary was built from {host}. Pass --vm-guest-runtime with a pinned prebuilt ELF"
    ))
}

pub(super) fn vm_guest_build_command(workspace: &Path) -> Command {
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

pub(super) fn vm_guest_runtime_is_fresh(workspace: &Path, runtime: &Path) -> Result<bool> {
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

pub(super) fn write_vm_guest_build_record(workspace: &Path, runtime: &Path) -> Result<()> {
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

pub(super) fn parse_cargo_dep_info(contents: &str) -> io::Result<Vec<PathBuf>> {
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
