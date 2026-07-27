use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use super::agents_md::{load_global_instructions, load_instructions};
use crate::{NanocodexError, Result};

#[derive(Clone, Default)]
pub(crate) struct ContextSourceConfig {
    codex_home: Option<PathBuf>,
}

impl ContextSourceConfig {
    pub(crate) fn set_codex_home(&mut self, codex_home: PathBuf) {
        self.codex_home = Some(codex_home);
    }

    pub(crate) fn codex_home(&self) -> Option<&Path> {
        self.codex_home.as_deref()
    }

    pub(crate) fn build(&self) -> ContextSource {
        ContextSource {
            global_instructions: load_global_instructions(self.codex_home()),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ContextSource {
    global_instructions: Option<Arc<str>>,
}

impl ContextSource {
    pub(crate) fn resolve_workspace(&self, requested: Option<&str>) -> Result<String> {
        let requested = PathBuf::from(requested.unwrap_or("."));
        let resolved = std::fs::canonicalize(&requested).map_err(|source| {
            NanocodexError::ResolveWorkspace {
                path: requested,
                source,
            }
        })?;
        if !resolved.is_dir() {
            return Err(NanocodexError::WorkspaceNotDirectory { path: resolved });
        }
        resolved
            .into_os_string()
            .into_string()
            .map_err(|path| NanocodexError::WorkspaceNotUtf8 {
                path: PathBuf::from(path),
            })
    }

    pub(crate) fn project_instructions(&self, workspace: &str) -> Result<Option<String>> {
        load_instructions(Path::new(workspace), self.global_instructions.as_deref())
    }

    pub(crate) fn global_instructions(&self) -> Option<Arc<str>> {
        self.global_instructions.as_ref().map(Arc::clone)
    }

    pub(crate) fn with_fallback_global(mut self, fallback: Option<Arc<str>>) -> Self {
        if self.global_instructions.is_none() {
            self.global_instructions = fallback;
        }
        self
    }
}
