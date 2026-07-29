use std::{fs, path::PathBuf};

use arcbox_ext4::Reader;
use nanocodex_vm::{
    host::EgressLease,
    image::{CachePolicy, DiskStatus, VmImageBuilder},
    tools::GuestRuntimeDisk,
};

const DISK_BYTES: u64 = 512 * 1024 * 1024;

#[tokio::test]
#[ignore = "requires a signed libkrun VMM, firmware, current guest ELF, and OCI cache"]
async fn run_instruction_uses_the_public_private_config_vmm_contract() {
    let vmm = required_path("NANOCODEX_VM_IMAGE_VMM");
    let guest = required_path("NANOCODEX_VM_IMAGE_RUNTIME");
    let firmware = required_path("NANOCODEX_VM_IMAGE_FIRMWARE");
    let cache = required_path("NANOCODEX_VM_IMAGE_CACHE");
    let runtime =
        GuestRuntimeDisk::prepare(guest, &cache).expect("content-addressed guest runtime disk");
    let context = tempfile::tempdir().expect("build context");
    fs::write(
        context.path().join("Dockerfile"),
        concat!(
            "FROM alpine:3.24\n",
            "RUN printf nanocodex-vm-image-live > /nanocodex-vm-image-proof && ",
            "printf %s \"$NANOCODEX_BUILD_EGRESS_PROOF\" > /nanocodex-vm-egress-proof\n",
            "WORKDIR /workspace\n",
        ),
    )
    .expect("Dockerfile");

    let mut egress = EgressLease::internet();
    egress
        .insert_environment("NANOCODEX_BUILD_EGRESS_PROOF", "inherited-by-run")
        .expect("build egress environment");
    egress
        .set_build_cache_scope("image-live-build-egress-v1")
        .expect("build egress cache scope");
    let builder = VmImageBuilder::new(vmm, runtime.path())
        .firmware_directory(firmware)
        .egress(egress)
        .vmm_arg("--vmm");
    let image = builder
        .prepare(context.path(), DISK_BYTES, &cache, CachePolicy::Reuse)
        .await
        .expect("prepared image");

    let mut disk = Reader::new(image.path()).expect("prepared ext4");
    assert_eq!(
        disk.read_file("/nanocodex-vm-image-proof", 0, Some(64))
            .expect("proof file"),
        b"nanocodex-vm-image-live"
    );
    assert_eq!(
        disk.read_file("/nanocodex-vm-egress-proof", 0, Some(64))
            .expect("egress proof file"),
        b"inherited-by-run"
    );
    assert!(
        !disk.exists("/run/nanocodex-build-resolver"),
        "build-only resolver state must not persist in the prepared image"
    );
    assert_eq!(image.workdir(), "/workspace");

    let warm = builder
        .prepare(context.path(), DISK_BYTES, &cache, CachePolicy::Reuse)
        .await
        .expect("warm prepared image");
    assert_eq!(warm.disk_status(), DiskStatus::Hit);
    assert_eq!(warm.path(), image.path());
}

fn required_path(name: &str) -> PathBuf {
    std::env::var_os(name).map_or_else(
        || panic!("{name} must name an existing live-test input"),
        PathBuf::from,
    )
}
