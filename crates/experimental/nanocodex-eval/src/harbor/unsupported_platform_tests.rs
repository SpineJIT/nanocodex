use tempfile::tempdir;

use super::{HarborArtifacts, HarborError};

#[test]
fn atomic_artifacts_fail_closed_without_durable_directory_sync() {
    let output = tempdir().unwrap();
    let target = output.path().join("result.json");

    let error = HarborArtifacts::atomic_write(&target, b"terminal\n").unwrap_err();

    assert!(
        matches!(error, HarborError::Io(error) if error.kind() == std::io::ErrorKind::Unsupported)
    );
    assert!(!target.exists());
}
