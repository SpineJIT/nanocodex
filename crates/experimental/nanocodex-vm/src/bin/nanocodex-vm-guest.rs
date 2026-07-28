#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), nanocodex_vm::tools::VmGuestError> {
    let workspace = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("/workspace"), PathBuf::from);
    nanocodex_vm::tools::serve_guest(workspace).await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nanocodex-vm-guest must be built for a Linux guest target");
}
