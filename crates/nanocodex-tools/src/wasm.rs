use std::{path::PathBuf, sync::Arc};

use js_sys::Promise;
use nanocodex_oai_api::{
    PromptInput, UserInput,
    auth::OpenAiAuth,
    responses::{ContentItem, CustomToolFormat, ResponseItem},
    tools::{ToolContext, ToolDefinition, ToolOutputBody},
};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

const EXEC_GRAMMAR: &str = r"start: /[\s\S]+/";
const EXEC_DESCRIPTION: &str = r"Run JavaScript in the embedded host.
- `tools` contains the application-defined async tools listed below.
- `text(value)` and `image(value)` append output for the model.
- `generatedImage(result)` appends an image-generation result for the model.
- `store(key, value)` and `load(key)` retain serializable values across calls.
- JavaScript runs inside the Node or browser host supplied by the embedding application.";

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = ["globalThis", "nanocodexHost"], js_name = executeCode)]
    fn host_execute_code(source: &str, session_id: &str, call_id: &str)
    -> Result<Promise, JsValue>;

    #[wasm_bindgen(js_namespace = ["globalThis", "nanocodexHost"], js_name = toolDefinitions)]
    fn host_tool_definitions(session_id: &str) -> String;
}

#[doc(hidden)]
pub struct OwnedToolContext {
    model: String,
    session_id: String,
    call_id: String,
    history: Arc<Vec<ResponseItem>>,
    output_token_budget: usize,
}

impl OwnedToolContext {
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        session_id: impl Into<String>,
        call_id: impl Into<String>,
        history: Arc<Vec<ResponseItem>>,
        output_token_budget: usize,
    ) -> Self {
        Self {
            model: model.into(),
            session_id: session_id.into(),
            call_id: call_id.into(),
            history,
            output_token_budget,
        }
    }

    fn borrowed(&self) -> ToolContext<'_> {
        ToolContext::new(
            &self.model,
            &self.session_id,
            &self.call_id,
            self.history.as_slice(),
            self.output_token_budget,
        )
    }
}

/// Complete result returned by the embedding host for one Code Mode cell.
pub struct CodeModeExecution {
    /// Ordered model-visible output emitted by the cell.
    pub output: ToolOutputBody,
    /// Whether the host reports successful execution.
    pub success: bool,
    /// Nested host tool calls in invocation order.
    pub nested_calls: Vec<NestedToolCall>,
    /// Application notifications emitted by the cell.
    pub notifications: Vec<CodeModeNotification>,
}

/// One notification emitted by a browser or Node Code Mode host.
pub struct CodeModeNotification {
    /// Code Mode call that emitted the notification.
    pub call_id: String,
    /// Complete notification text.
    pub text: String,
}

/// Incremental nested-tool update replayed from a host execution.
pub enum CodeModeUpdate<'a> {
    /// A nested call started.
    NestedCallStarted {
        /// Stable nested call identity.
        call_id: &'a str,
        /// Host tool name.
        name: &'a str,
        /// Complete JSON input value.
        input: &'a Value,
    },
    /// A nested call reached its terminal result.
    NestedCallCompleted(&'a NestedToolCall),
}

#[doc(hidden)]
pub trait CodeModeObserver {
    fn update(&mut self, update: CodeModeUpdate<'_>);
}

#[derive(Deserialize)]
/// Recorded nested tool call returned by the embedding host.
pub struct NestedToolCall {
    /// Stable nested call identity.
    pub call_id: String,
    /// Host tool name.
    pub name: String,
    /// Complete JSON input value.
    pub input: Value,
    /// Complete model-visible output.
    pub output: ToolOutputBody,
    /// Whether the nested call succeeded.
    pub success: bool,
    /// Nanoseconds from cell start until this call started.
    pub started_after_ns: u64,
    /// Nanoseconds spent executing this call.
    pub duration_ns: u64,
    #[serde(default)]
    /// Optional opaque metadata retained for events and adapters.
    pub metadata: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostCodeExecution {
    output: ToolOutputBody,
    success: bool,
    #[serde(default)]
    nested_calls: Vec<NestedToolCall>,
    #[serde(default)]
    notifications: Vec<HostNotification>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostNotification {
    call_id: String,
    text: String,
}

/// Web-search connection inputs retained by the host-backed WASM runtime.
pub struct WebSearchConfig {
    /// Complete search endpoint URL.
    pub endpoint: String,
    /// `OpenAI` authentication source.
    pub auth: OpenAiAuth,
}

/// Image-generation inputs retained by the host-backed WASM runtime.
pub struct ImageGenerationConfig {
    /// Base `OpenAI` API URL.
    pub api_base_url: String,
    /// `OpenAI` authentication source.
    pub auth: OpenAiAuth,
    /// Host-side image persistence root.
    pub save_root: PathBuf,
}

/// Browser tool selection.
///
/// Browser and Node tools are supplied by `globalThis.nanocodexHost`; native
/// built-in selection is intentionally unavailable on WASM.
#[derive(Clone, Default)]
pub struct Tools;

impl Tools {
    /// Returns `false`; native direct web search is unavailable on WASM.
    #[must_use]
    pub const fn web_search_enabled(&self) -> bool {
        false
    }

    /// Returns `false`; native image generation is unavailable on WASM.
    #[must_use]
    pub const fn image_generation_enabled(&self) -> bool {
        false
    }
}

/// WASM Code Mode adapter over `globalThis.nanocodexHost`.
pub struct ToolRuntime {
    working_directory: Arc<str>,
}

#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct ToolRuntimeControl;

impl ToolRuntime {
    /// Creates a host-backed runtime.
    ///
    /// HTTP-tool configurations are ignored because the embedding host owns
    /// all executable tools.
    pub fn new(
        workspace: impl Into<PathBuf>,
        _web_search: Option<WebSearchConfig>,
        _image_generation: Option<ImageGenerationConfig>,
    ) -> Self {
        let workspace = workspace.into();
        Self {
            working_directory: Arc::from(workspace.to_string_lossy().into_owned()),
        }
    }

    /// Builds the browser runtime from the complete declarative tool selection.
    #[must_use]
    pub fn new_with_tools(
        workspace: impl Into<PathBuf>,
        web_search: Option<WebSearchConfig>,
        image_generation: Option<ImageGenerationConfig>,
        tools: &Tools,
    ) -> Self {
        Self::new(workspace, web_search, image_generation).with_tools(tools)
    }

    /// Applies a browser tool selection.
    ///
    /// Host definitions remain authoritative, so the current selection is a
    /// no-op on WASM.
    #[must_use]
    pub const fn with_tools(self, _tools: &Tools) -> Self {
        self
    }

    /// Returns the fixed model-visible runtime name.
    #[must_use]
    pub const fn default_shell_name(&self) -> &'static str {
        "javascript"
    }

    /// Returns the model-visible working directory supplied at construction.
    #[must_use]
    pub fn working_directory(&self) -> &str {
        &self.working_directory
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn control(&self) -> ToolRuntimeControl {
        ToolRuntimeControl
    }

    /// Builds the `exec` definition from the host's current tool definitions.
    #[must_use]
    pub fn model_specs(&self, session_id: &str) -> Vec<ToolDefinition> {
        let definitions =
            serde_json::from_str::<Vec<ToolDefinition>>(&host_tool_definitions(session_id))
                .unwrap_or_default();
        let mut description = EXEC_DESCRIPTION.to_owned();
        for definition in definitions {
            description.push_str("\n\n- `tools.");
            description.push_str(definition.name());
            description.push_str("`: ");
            description.push_str(definition.description().trim());
        }
        vec![ToolDefinition::custom(
            "exec",
            description,
            CustomToolFormat::grammar("lark", EXEC_GRAMMAR),
        )]
    }

    /// Executes one JavaScript cell through the embedding host.
    pub async fn execute_code(&self, source: &str, context: ToolContext<'_>) -> CodeModeExecution {
        let promise = match host_execute_code(source, context.session_id(), context.call_id()) {
            Ok(promise) => promise,
            Err(error) => return failed(&js_error(&error)),
        };
        let value = match JsFuture::from(promise).await {
            Ok(value) => value,
            Err(error) => return failed(&js_error(&error)),
        };
        let Some(encoded) = value.as_string() else {
            return failed("JavaScript code-mode host returned a non-string result");
        };
        match serde_json::from_str::<HostCodeExecution>(&encoded) {
            Ok(execution) => CodeModeExecution {
                output: execution.output,
                success: execution.success,
                nested_calls: execution.nested_calls,
                notifications: execution
                    .notifications
                    .into_iter()
                    .map(|notification| CodeModeNotification {
                        call_id: notification.call_id,
                        text: notification.text,
                    })
                    .collect(),
            },
            Err(error) => failed(&format!(
                "JavaScript code-mode host returned invalid JSON: {error}"
            )),
        }
    }

    #[doc(hidden)]
    pub async fn execute_code_owned(
        &self,
        source: &str,
        context: OwnedToolContext,
    ) -> CodeModeExecution {
        self.execute_code(source, context.borrowed()).await
    }

    #[doc(hidden)]
    pub async fn execute_code_owned_with_updates(
        &self,
        source: &str,
        context: OwnedToolContext,
        observer: &mut dyn CodeModeObserver,
    ) -> CodeModeExecution {
        let execution = self.execute_code_owned(source, context).await;
        replay_nested_updates(&execution, observer);
        execution
    }

    /// Returns a failed result because yielded cells are unsupported by this adapter.
    #[expect(
        clippy::unused_async,
        reason = "matches the native tool-runtime contract"
    )]
    pub async fn wait_for_code(
        &self,
        _input: &str,
        _context: ToolContext<'_>,
    ) -> CodeModeExecution {
        failed("background code-mode cells are unavailable in the WASM runtime")
    }

    #[doc(hidden)]
    pub async fn wait_for_code_with_updates(
        &self,
        input: &str,
        context: ToolContext<'_>,
        observer: &mut dyn CodeModeObserver,
    ) -> CodeModeExecution {
        let execution = self.wait_for_code(input, context).await;
        replay_nested_updates(&execution, observer);
        execution
    }
}

impl ToolRuntimeControl {
    #[doc(hidden)]
    #[expect(
        clippy::unused_async,
        reason = "matches the native tool-runtime control contract"
    )]
    pub async fn cancel(&self) {}
}

fn replay_nested_updates(execution: &CodeModeExecution, observer: &mut dyn CodeModeObserver) {
    for call in &execution.nested_calls {
        observer.update(CodeModeUpdate::NestedCallStarted {
            call_id: &call.call_id,
            name: &call.name,
            input: &call.input,
        });
        observer.update(CodeModeUpdate::NestedCallCompleted(call));
    }
}

#[expect(
    clippy::unused_async,
    reason = "matches the native input-preparation contract"
)]
/// Converts public prompt input to provider-ready content without native image processing.
pub async fn prepare_user_input(input: &PromptInput) -> Vec<ContentItem> {
    let items = match input {
        PromptInput::Text(text) => vec![UserInput::Text { text: text.clone() }],
        PromptInput::Content(items) => items.clone(),
    };
    items
        .into_iter()
        .map(|item| match item {
            UserInput::Text { text } => ContentItem::InputText {
                text: text.into_boxed_str(),
            },
            UserInput::Image { image_url, detail } => ContentItem::InputImage {
                image_url: image_url.into_boxed_str(),
                detail,
            },
            UserInput::Audio { audio_url } => ContentItem::InputAudio {
                audio_url: audio_url.into_boxed_str(),
            },
            UserInput::LocalImage { path, .. } => ContentItem::InputText {
                text: format!(
                    "Local image paths are unavailable in browser WASM: {}",
                    path.display()
                )
                .into_boxed_str(),
            },
            UserInput::LocalAudio { path } => ContentItem::InputText {
                text: format!(
                    "Local audio paths are unavailable in browser WASM: {}",
                    path.display()
                )
                .into_boxed_str(),
            },
        })
        .collect()
}

#[expect(
    clippy::unused_async,
    reason = "matches the native output-preparation contract"
)]
/// Leaves host-prepared tool images unchanged.
pub async fn prepare_output_images(_output: &mut ToolOutputBody) {}

fn failed(message: &str) -> CodeModeExecution {
    CodeModeExecution {
        output: ToolOutputBody::Text(format!("Script failed\nOutput:\n{message}")),
        success: false,
        nested_calls: Vec::new(),
        notifications: Vec::new(),
    }
}

fn js_error(error: &JsValue) -> String {
    error.as_string().unwrap_or_else(|| format!("{error:?}"))
}
