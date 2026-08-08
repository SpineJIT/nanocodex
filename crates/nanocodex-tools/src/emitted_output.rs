use crate::contract::{ToolOutputBody, ToolOutputContent};

const MAX_EMITTED_OUTPUT_BYTES: usize = 1_000;
const TRUNCATION_MARKER: &str = "\n[emitted output truncated]\n";
const NON_TEXT_MARKER: &str = "[non-text emitted output omitted]";

pub(crate) fn bounded_emitted_output<'a>(
    outputs: impl IntoIterator<Item = &'a ToolOutputBody>,
) -> Option<ToolOutputBody> {
    let mut text = String::new();
    for output in outputs {
        if !text.is_empty() {
            text.push('\n');
        }
        append_output_text(&mut text, output);
    }
    (!text.is_empty()).then(|| ToolOutputBody::Text(truncate(text)))
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn append_to_content(content: &mut Vec<ToolOutputContent>, output: ToolOutputBody) {
    match output {
        ToolOutputBody::Text(text) => content.push(ToolOutputContent::InputText { text }),
        ToolOutputBody::Content(items) => content.extend(items),
    }
}

fn append_output_text(target: &mut String, output: &ToolOutputBody) {
    match output {
        ToolOutputBody::Text(text) => target.push_str(text),
        ToolOutputBody::Content(items) => {
            for item in items {
                match item {
                    ToolOutputContent::InputText { text } => target.push_str(text),
                    ToolOutputContent::InputImage { .. } | ToolOutputContent::InputAudio { .. } => {
                        target.push_str(NON_TEXT_MARKER);
                    }
                }
            }
        }
    }
}

fn truncate(text: String) -> String {
    if text.len() <= MAX_EMITTED_OUTPUT_BYTES {
        return text;
    }
    let limit = MAX_EMITTED_OUTPUT_BYTES.saturating_sub(TRUNCATION_MARKER.len());
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{TRUNCATION_MARKER}", &text[..end])
}
