use std::sync::Arc;

use crate::Result;

#[derive(Clone, Default)]
pub(crate) struct ContextSourceConfig;

impl ContextSourceConfig {
    pub(crate) fn build(&self) -> ContextSource {
        ContextSource
    }
}

#[derive(Clone)]
pub(crate) struct ContextSource;

impl ContextSource {
    pub(crate) fn resolve_workspace(&self, requested: Option<&str>) -> Result<String> {
        Ok(requested.unwrap_or(".").to_owned())
    }

    pub(crate) fn project_instructions(&self, _workspace: &str) -> Option<String> {
        None
    }

    pub(crate) fn global_instructions(&self) -> Option<Arc<str>> {
        None
    }

    pub(crate) fn with_fallback_global(self, _fallback: Option<Arc<str>>) -> Self {
        self
    }
}
