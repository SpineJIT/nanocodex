use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use nanocodex_oai_api::ToolDefinition;
use serde::Deserialize;
use serde_json::json;

use super::{
    ImageDetail, StandardTool, Tool, ToolContext, ToolExecution, ToolInput, ToolOutputContent,
    ToolResult,
};

pub(super) struct ViewImageHandler {
    workspace: PathBuf,
}

impl ViewImageHandler {
    pub(super) fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for ViewImageHandler {
    fn definition(&self) -> ToolDefinition {
        StandardTool::ViewImage.definition()
    }

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let arguments = input.decode_json::<ViewImageArguments>()?;
        let detail = match arguments.detail.as_deref() {
            None | Some("high") => ImageDetail::High,
            Some("original") => ImageDetail::Original,
            Some(detail) => {
                return Ok(ToolExecution::error(format!(
                    "view_image.detail only supports `high` or `original`; omit `detail` for default high resized behavior, got `{detail}`"
                )));
            }
        };
        let path = resolve(&self.workspace, Path::new(&arguments.path));
        match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Ok(ToolExecution::error(format!(
                    "image path `{}` is not a file",
                    path.display()
                )));
            }
            Err(error) => {
                return Ok(ToolExecution::error(format!(
                    "unable to locate image at `{}`: {error}",
                    path.display()
                )));
            }
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(ToolExecution::error(format!(
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
        Ok(ToolExecution::content(vec![ToolOutputContent::InputImage {
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
