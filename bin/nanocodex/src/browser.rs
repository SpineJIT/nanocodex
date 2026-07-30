use std::path::{Path, PathBuf};

use clap::{ArgAction, Args};
use eyre::{Result, WrapErr};
use nanocodex_browser::{Browser, BrowserTool};

/// Opt-in local browser configuration for normal agent sessions.
#[derive(Args, Default)]
pub(crate) struct BrowserArgs {
    /// Expose one private Chromium session to Code Mode as `tools.browser`.
    ///
    /// Chromium starts lazily on the first browser action and remains alive for
    /// the complete agent session.
    #[arg(
        long,
        env = "NANOCODEX_BROWSER",
        action = ArgAction::SetTrue
    )]
    browser: bool,

    /// Chrome or Chromium executable used by the browser tool.
    #[arg(
        long,
        env = "NANOCODEX_BROWSER_EXECUTABLE",
        value_name = "PATH",
        requires = "browser"
    )]
    browser_executable: Option<PathBuf>,
}

pub(crate) struct ConfiguredBrowser {
    browser: Browser,
}

impl BrowserArgs {
    #[cfg(test)]
    pub(crate) const fn is_enabled(&self) -> bool {
        self.browser
    }

    pub(crate) fn configure(&self, workspace: &Path) -> Result<Option<ConfiguredBrowser>> {
        if !self.browser {
            return Ok(None);
        }
        let mut builder = Browser::builder().file_root(workspace);
        if let Some(executable) = &self.browser_executable {
            builder = builder.executable(executable);
        }
        let browser = builder
            .build()
            .wrap_err("failed to configure the browser tool")?;
        Ok(Some(ConfiguredBrowser { browser }))
    }
}

impl ConfiguredBrowser {
    pub(crate) fn tool(&self) -> BrowserTool {
        BrowserTool::from_browser(self.browser.clone())
    }

    pub(crate) async fn shutdown(self) -> Result<()> {
        self.browser
            .close()
            .await
            .wrap_err("failed to shut down the browser tool")
    }
}

#[cfg(test)]
mod tests {
    use nanocodex::Tools;
    use nanocodex_tools::runtime::ToolRuntime;

    use super::BrowserArgs;

    #[tokio::test]
    async fn configured_browser_adds_no_model_facing_schema() {
        let workspace = tempfile::tempdir().unwrap();
        let baseline_tools = Tools::builder().build().unwrap();
        let baseline = ToolRuntime::new_with_tools(workspace.path(), None, None, &baseline_tools)
            .model_specs("browser-tui-test");
        let browser = BrowserArgs {
            browser: true,
            browser_executable: None,
        }
        .configure(workspace.path())
        .unwrap()
        .unwrap();
        let tools = Tools::builder().provider(browser.tool()).build().unwrap();
        let runtime = ToolRuntime::new_with_tools(workspace.path(), None, None, &tools);
        let definitions = runtime.model_specs("browser-tui-test");
        let serialized = serde_json::to_string(&definitions).unwrap();

        assert_eq!(
            serde_json::to_vec(&definitions).unwrap(),
            serde_json::to_vec(&baseline).unwrap(),
            "enabling browser must not add any model-input schema bytes"
        );
        assert!(!serialized.contains("browser"));
        assert!(runtime.contains("browser"));
        browser.shutdown().await.unwrap();
    }
}
