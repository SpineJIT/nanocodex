use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use eyre::{Context, Result, bail, eyre};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const CHECKSUM_FILE: &str = "nanocodex.sha256";

#[cfg(windows)]
const BINARY_NAME: &str = "nanocodex.exe";
#[cfg(not(windows))]
const BINARY_NAME: &str = "nanocodex";

pub(super) struct VersionStore {
    root: PathBuf,
}

impl VersionStore {
    pub(super) fn discover() -> Result<Self> {
        let root = if let Some(root) = std::env::var_os("NANOCODEX_DIR") {
            PathBuf::from(root)
        } else {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .ok_or_else(|| eyre!("HOME is not set; set NANOCODEX_DIR explicitly"))?;
            PathBuf::from(home).join(".nanocodex")
        };
        if root.as_os_str().is_empty() {
            bail!("NANOCODEX_DIR cannot be empty");
        }
        Ok(Self { root })
    }

    #[cfg(test)]
    fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub(super) fn prepare(&self, manager_version: &str) -> Result<()> {
        let executable = std::env::current_exe()
            .wrap_err("failed to locate the running Nanocodex executable")?;
        let contents = fs::read(&executable)
            .wrap_err_with(|| format!("failed to read {}", executable.display()))?;
        self.prepare_with_contents(manager_version, &contents)
    }

    fn prepare_with_contents(&self, manager_version: &str, contents: &[u8]) -> Result<()> {
        validate_key(manager_version)?;
        fs::create_dir_all(self.versions_dir())
            .wrap_err("failed to create the Nanocodex version store")?;
        fs::create_dir_all(self.root.join("updater"))
            .wrap_err("failed to create the Nanocodex updater directory")?;
        fs::create_dir_all(self.root.join("bin"))
            .wrap_err("failed to create the Nanocodex bin directory")?;

        let active = self.active()?;
        let updater_exists = self.updater_path().is_file();
        if (!updater_exists || active.is_none()) && !self.is_cached(manager_version)? {
            self.install(manager_version, contents)?;
        }
        if !updater_exists {
            atomic_write(&self.updater_path(), contents, true)?;
        }
        if active.is_none() {
            self.activate(manager_version)?;
        }

        #[cfg(unix)]
        self.install_launcher()?;

        Ok(())
    }

    pub(super) fn is_cached(&self, key: &str) -> Result<bool> {
        validate_key(key)?;
        let binary = self.binary_path(key);
        let checksum_path = self.checksum_path(key);
        if !binary.is_file() || !checksum_path.is_file() {
            return Ok(false);
        }
        let expected = fs::read_to_string(&checksum_path)
            .wrap_err_with(|| format!("failed to read {}", checksum_path.display()))?;
        let expected = expected.trim();
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(false);
        }
        let contents = fs::read(&binary)
            .wrap_err_with(|| format!("failed to read cached {}", binary.display()))?;
        Ok(hex::encode(Sha256::digest(contents)) == expected.to_ascii_lowercase())
    }

    pub(super) fn install(&self, key: &str, contents: &[u8]) -> Result<()> {
        validate_key(key)?;
        let directory = self.version_dir(key);
        fs::create_dir_all(&directory)
            .wrap_err_with(|| format!("failed to create {}", directory.display()))?;
        atomic_write(&self.binary_path(key), contents, true)?;
        let checksum = hex::encode(Sha256::digest(contents));
        atomic_write(
            &self.checksum_path(key),
            format!("{checksum}\n").as_bytes(),
            false,
        )
    }

    pub(super) fn activate(&self, key: &str) -> Result<()> {
        if !self.is_cached(key)? {
            bail!("Nanocodex version {key} is not installed or its checksum is invalid");
        }

        #[cfg(unix)]
        {
            self.activate_symlink(key)?;
            self.install_launcher()?;
        }

        #[cfg(not(unix))]
        {
            self_replace::self_replace(self.binary_path(key)).wrap_err(
                "failed to replace the running Nanocodex executable with the selected version",
            )?;
            atomic_write(
                &self.root.join("active-version"),
                format!("{key}\n").as_bytes(),
                false,
            )?;
        }

        Ok(())
    }

    pub(super) fn active(&self) -> Result<Option<String>> {
        #[cfg(unix)]
        {
            let target = match fs::read_link(self.root.join("current")) {
                Ok(target) => target,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error).wrap_err("failed to read the active Nanocodex link");
                }
            };
            target
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .ok_or_else(|| eyre!("the active Nanocodex link has an invalid target"))
                .map(Some)
        }

        #[cfg(not(unix))]
        {
            let path = self.root.join("active-version");
            match fs::read_to_string(&path) {
                Ok(key) => Ok(Some(key.trim().to_owned())),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => {
                    Err(error).wrap_err_with(|| format!("failed to read {}", path.display()))
                }
            }
        }
    }

    pub(super) fn promote_manager(&self, key: &str) -> Result<()> {
        if !self.is_cached(key)? {
            bail!("cannot promote missing Nanocodex version {key} to updater");
        }

        #[cfg(unix)]
        {
            let contents = fs::read(self.binary_path(key))
                .wrap_err_with(|| format!("failed to read Nanocodex version {key}"))?;
            atomic_write(&self.updater_path(), &contents, true)?;
        }

        Ok(())
    }

    fn versions_dir(&self) -> PathBuf {
        self.root.join("versions")
    }

    fn version_dir(&self, key: &str) -> PathBuf {
        self.versions_dir().join(key)
    }

    fn binary_path(&self, key: &str) -> PathBuf {
        self.version_dir(key).join(BINARY_NAME)
    }

    fn checksum_path(&self, key: &str) -> PathBuf {
        self.version_dir(key).join(CHECKSUM_FILE)
    }

    fn updater_path(&self) -> PathBuf {
        self.root.join("updater").join(BINARY_NAME)
    }

    #[cfg(unix)]
    fn activate_symlink(&self, key: &str) -> Result<()> {
        use std::os::unix::fs::symlink;

        let current = self.root.join("current");
        let temporary = self.root.join(format!(".current-{}", std::process::id()));
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to remove {}", temporary.display()));
            }
        }
        symlink(Path::new("versions").join(key), &temporary)
            .wrap_err("failed to create the active Nanocodex link")?;
        if let Err(error) = fs::rename(&temporary, &current) {
            let _ = fs::remove_file(&temporary);
            return Err(error).wrap_err("failed to activate the selected Nanocodex version");
        }
        Ok(())
    }

    #[cfg(unix)]
    fn install_launcher(&self) -> Result<()> {
        const LAUNCHER: &str = r#"#!/bin/sh
set -eu

case "$0" in
    */*) launcher=$0 ;;
    *) launcher=$(command -v "$0") ;;
esac
bin_dir=$(CDPATH= cd -- "$(dirname -- "$launcher")" && pwd -P)
install_root=$(dirname -- "$bin_dir")

if [ "${1-}" = "update" ]; then
    export NANOCODEX_DIR="$install_root"
    exec "$install_root/updater/nanocodex" "$@"
fi
exec "$install_root/current/nanocodex" "$@"
"#;

        let path = self.root.join("bin").join(BINARY_NAME);
        if fs::read(&path).is_ok_and(|contents| contents == LAUNCHER.as_bytes()) {
            return Ok(());
        }
        atomic_write(&path, LAUNCHER.as_bytes(), true)
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.starts_with('.')
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
    {
        bail!("invalid Nanocodex version key {key:?}");
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8], executable: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    let mut temporary =
        NamedTempFile::new_in(parent).wrap_err("failed to create a temporary install file")?;
    temporary
        .write_all(contents)
        .wrap_err_with(|| format!("failed to write {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .wrap_err_with(|| format!("failed to sync {}", path.display()))?;

    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;

        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .wrap_err_with(|| format!("failed to make {} executable", path.display()))?;
    }

    #[cfg(not(unix))]
    let _ = executable;

    temporary
        .persist(path)
        .map_err(|error| error.error)
        .wrap_err_with(|| format!("failed to install {}", path.display()))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn retains_versions_and_switches_the_active_link() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        store.prepare_with_contents("0.3.0", b"current").unwrap();

        assert_eq!(store.active().unwrap().as_deref(), Some("0.3.0"));
        assert_eq!(fs::read(store.binary_path("0.3.0")).unwrap(), b"current");
        assert_eq!(fs::read(store.updater_path()).unwrap(), b"current");
        let launcher = fs::read_to_string(directory.path().join("bin/nanocodex")).unwrap();
        assert!(launcher.contains("updater/nanocodex"));
        assert!(launcher.contains("export NANOCODEX_DIR"));

        store.install("0.2.0", b"previous").unwrap();
        store.activate("0.2.0").unwrap();

        assert_eq!(store.active().unwrap().as_deref(), Some("0.2.0"));
        assert_eq!(fs::read(store.binary_path("0.2.0")).unwrap(), b"previous");
        assert_eq!(fs::read(store.binary_path("0.3.0")).unwrap(), b"current");
    }

    #[test]
    fn refuses_corrupted_cached_versions() {
        let directory = tempfile::tempdir().unwrap();
        let store = VersionStore::at(directory.path());
        store.install("0.2.0", b"original").unwrap();
        assert!(store.is_cached("0.2.0").unwrap());

        fs::write(store.binary_path("0.2.0"), b"corrupted").unwrap();

        assert!(!store.is_cached("0.2.0").unwrap());
        assert!(store.activate("0.2.0").is_err());
    }
}
