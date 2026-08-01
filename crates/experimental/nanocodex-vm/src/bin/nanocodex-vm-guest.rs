#[cfg(target_os = "linux")]
use std::{ffi::OsStr, io, path::PathBuf};

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    if first.as_deref() == Some(OsStr::new("--overlay-root")) {
        let workspace = arguments.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "--overlay-root requires a guest workspace",
            )
        })?;
        let resolver = arguments.next().unwrap_or_default();
        if arguments.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--overlay-root accepts only WORKSPACE and optional RESOLVER",
            )
            .into());
        }
        let resolver = resolver.to_string_lossy();
        return nanocodex_vm::tools::serve_overlay_guest(
            PathBuf::from(workspace),
            (!resolver.is_empty()).then_some(resolver.as_ref()),
        )
        .await
        .map_err(Into::into);
    }

    let workspace = first.map_or_else(|| PathBuf::from("/workspace"), PathBuf::from);
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guest runtime accepts only one workspace argument",
        )
        .into());
    }
    nanocodex_vm::tools::serve_guest(workspace)
        .await
        .map_err(Into::into)
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nanocodex-vm-guest must be built for a Linux guest target");
}
