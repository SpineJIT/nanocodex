use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use nanocodex_oai_api::tools::ToolDefinition;
use serde::Deserialize;
use serde_json::json;

use super::{
    ImageDetail, StandardTool, Tool, ToolContext, ToolInput, ToolOutput, ToolOutputContent,
    ToolResult,
};

pub(super) struct ViewImageHandler {
    workspace: PathBuf,
}

impl ViewImageHandler {
    pub(super) const fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for ViewImageHandler {
    fn definition(&self) -> ToolDefinition {
        StandardTool::ViewImage.definition()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let arguments = input.decode_json::<ViewImageArguments>()?;
        let detail = match arguments.detail.as_deref() {
            None | Some("high") => ImageDetail::High,
            Some("original") => ImageDetail::Original,
            Some(detail) => {
                return Ok(ToolOutput::error(format!(
                    "view_image.detail only supports `high` or `original`; omit `detail` for default high resized behavior, got `{detail}`"
                )));
            }
        };
        let path = resolve(&self.workspace, Path::new(&arguments.path));
        match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Ok(ToolOutput::error(format!(
                    "image path `{}` is not a file",
                    path.display()
                )));
            }
            Err(error) => {
                return Ok(ToolOutput::error(format!(
                    "unable to locate image at `{}`: {error}",
                    path.display()
                )));
            }
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(ToolOutput::error(format!(
                    "unable to read image at `{}`: {error}",
                    path.display()
                )));
            }
        };
        // The model-history boundary owns image validation, resizing, and caching.
        let image_url = format!(
            "data:application/octet-stream;base64,{}",
            STANDARD.encode(bytes)
        );
        Ok(ToolOutput::content(vec![ToolOutputContent::InputImage {
            image_url: image_url.clone(),
            detail,
        }])
        .with_code_mode_value(json!({
            "image_url": image_url,
            "detail": detail,
        })))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewImageArguments {
    path: String,
    #[serde(default)]
    detail: Option<String>,
}

fn resolve(workspace: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        workspace.join(path)
    }
}
