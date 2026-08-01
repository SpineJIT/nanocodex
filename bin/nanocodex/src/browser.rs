use std::path::{Path, PathBuf};

use clap::{ArgAction, Args, ValueEnum};
use eyre::{Result, WrapErr, eyre};
use nanocodex_browser::{BraveSession, Browser, BrowserTool};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum BrowserKind {
    #[value(alias = "true")]
    Chromium,
    Brave,
}

/// Opt-in local browser configuration for normal agent sessions.
#[derive(Args, Default)]
pub(crate) struct BrowserArgs {
    /// Expose one private browser session to Code Mode as `tools.browser`.
    ///
    /// Pass `brave` to use the standard Brave installation. A bare `--browser`
    /// preserves the private Chromium default.
    #[arg(
        long,
        env = "NANOCODEX_BROWSER",
        value_enum,
        num_args = 0..=1,
        default_missing_value = "chromium",
        require_equals = true
    )]
    browser: Option<BrowserKind>,

    /// Copy every cookie from the standard Brave profile into the selected browser.
    #[arg(
        long,
        env = "NANOCODEX_BROWSER_COOKIES",
        action = ArgAction::Set,
        default_value_t = false,
        requires = "browser"
    )]
    cookies: bool,

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
        self.browser.is_some()
    }

    pub(crate) fn configure(&self, workspace: &Path) -> Result<Option<ConfiguredBrowser>> {
        let Some(kind) = self.browser else {
            return Ok(None);
        };
        let mut builder = Browser::builder().file_root(workspace);
        match kind {
            BrowserKind::Chromium => {
                if let Some(executable) = &self.browser_executable {
                    builder = builder.executable(executable);
                }
                if self.cookies {
                    let brave = BraveSession::standard()
                        .wrap_err("failed to locate the standard Brave profile")?;
                    builder = builder.brave_cookie_source(brave.copy_all_cookies());
                }
            }
            BrowserKind::Brave => {
                if self.browser_executable.is_some() {
                    return Err(eyre!(
                        "--browser-executable cannot be combined with --browser=brave"
                    ));
                }
                let brave = BraveSession::standard()
                    .wrap_err("failed to locate the standard Brave profile")?;
                builder = if self.cookies {
                    builder.brave_session(brave.copy_all_cookies())
                } else {
                    builder.executable(brave.executable().to_path_buf())
                };
            }
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
            browser: Some(super::BrowserKind::Chromium),
            cookies: false,
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
