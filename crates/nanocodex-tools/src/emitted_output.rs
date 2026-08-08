use crate::contract::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolOutputBody, ToolOutputContent};

const MAX_EMITTED_OUTPUT_BYTES: usize = 1_000;
const MAX_MODEL_VISIBLE_OUTPUT_BYTES: usize = DEFAULT_TOOL_OUTPUT_TOKENS * 4;
const TRUNCATION_MARKER: &str = "\n[emitted output truncated]\n";
const NON_TEXT_MARKER: &str = "[non-text emitted output omitted]";

pub(crate) fn bounded_emitted_output<'a>(
    outputs: impl IntoIterator<Item = &'a ToolOutputBody>,
) -> Option<ToolOutputBody> {
    let mut text = BoundedText::default();
    for output in outputs {
        if !text.is_empty() {
            text.push("\n");
        }
        append_output_text(&mut text, output);
    }
    text.finish().map(ToolOutputBody::Text)
}

pub(crate) fn append_bounded_output(output: &mut ToolOutputBody, emitted: ToolOutputBody) {
    let emitted_text_bytes = text_bytes(&emitted);
    truncate_normal_output(
        output,
        MAX_MODEL_VISIBLE_OUTPUT_BYTES.saturating_sub(emitted_text_bytes),
    );
    let emitted_content = match emitted {
        ToolOutputBody::Text(text) => vec![ToolOutputContent::InputText { text }],
        ToolOutputBody::Content(content) => content,
    };
    match output {
        ToolOutputBody::Text(text) => {
            let normal = std::mem::take(text);
            let mut content = Vec::with_capacity(1 + emitted_content.len());
            content.push(ToolOutputContent::InputText { text: normal });
            content.extend(emitted_content);
            *output = ToolOutputBody::Content(content);
        }
        ToolOutputBody::Content(content) => content.extend(emitted_content),
    }
}

#[derive(Default)]
struct BoundedText {
    text: String,
    truncated: bool,
}

impl BoundedText {
    const fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn push(&mut self, value: &str) {
        if self.truncated || value.is_empty() {
            return;
        }
        let limit = MAX_EMITTED_OUTPUT_BYTES.saturating_sub(TRUNCATION_MARKER.len());
        let remaining = limit.saturating_sub(self.text.len());
        if value.len() <= remaining {
            self.text.push_str(value);
            return;
        }
        let end = floor_char_boundary(value, remaining);
        self.text.push_str(&value[..end]);
        self.truncated = true;
    }

    fn finish(mut self) -> Option<String> {
        if self.truncated {
            self.text.push_str(TRUNCATION_MARKER);
        }
        (!self.text.is_empty()).then_some(self.text)
    }
}

fn append_output_text(target: &mut BoundedText, output: &ToolOutputBody) {
    match output {
        ToolOutputBody::Text(text) => target.push(text),
        ToolOutputBody::Content(items) => {
            for item in items {
                match item {
                    ToolOutputContent::InputText { text } => target.push(text),
                    ToolOutputContent::InputImage { .. } | ToolOutputContent::InputAudio { .. } => {
                        target.push(NON_TEXT_MARKER);
                    }
                }
            }
        }
    }
}

fn text_bytes(output: &ToolOutputBody) -> usize {
    match output {
        ToolOutputBody::Text(text) => text.len(),
        ToolOutputBody::Content(content) => content
            .iter()
            .filter_map(|item| match item {
                ToolOutputContent::InputText { text } => Some(text.len()),
                ToolOutputContent::InputImage { .. } | ToolOutputContent::InputAudio { .. } => None,
            })
            .sum(),
    }
}

fn truncate_normal_output(output: &mut ToolOutputBody, mut remaining: usize) {
    match output {
        ToolOutputBody::Text(text) => text.truncate(floor_char_boundary(text, remaining)),
        ToolOutputBody::Content(content) => {
            let mut truncated = Vec::with_capacity(content.len());
            for item in std::mem::take(content) {
                match item {
                    ToolOutputContent::InputText { text } => {
                        let end = floor_char_boundary(&text, remaining);
                        if end > 0 {
                            remaining = remaining.saturating_sub(end);
                            truncated.push(ToolOutputContent::InputText {
                                text: text[..end].to_owned(),
                            });
                        }
                    }
                    item @ (ToolOutputContent::InputImage { .. }
                    | ToolOutputContent::InputAudio { .. }) => truncated.push(item),
                }
            }
            *content = truncated;
        }
    }
}

fn floor_char_boundary(text: &str, target: usize) -> usize {
    let mut boundary = target.min(text.len());
    while !text.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    boundary
}

#[cfg(test)]
mod tests {
    use crate::{ImageDetail, ToolOutputBody, ToolOutputContent};

    use super::{
        MAX_EMITTED_OUTPUT_BYTES, NON_TEXT_MARKER, TRUNCATION_MARKER, bounded_emitted_output,
    };

    #[test]
    fn emitted_output_bounds_utf8_text_and_omits_media_payloads() {
        let unicode = ToolOutputBody::Text("界".repeat(200));
        let media = ToolOutputBody::Content(vec![
            ToolOutputContent::InputImage {
                image_url: format!("data:image/png;base64,{}", "x".repeat(20_000)),
                detail: ImageDetail::High,
            },
            ToolOutputContent::InputAudio {
                audio_url: format!("data:audio/wav;base64,{}", "y".repeat(20_000)),
            },
        ]);

        let tail = ToolOutputBody::Text("語".repeat(500));
        let ToolOutputBody::Text(output) =
            bounded_emitted_output([&unicode, &media, &tail]).expect("output should be emitted")
        else {
            panic!("emitted output should be text");
        };
        assert!(output.len() <= MAX_EMITTED_OUTPUT_BYTES);
        assert!(output.is_char_boundary(output.len()));
        assert!(output.contains(TRUNCATION_MARKER));
        assert!(!output.contains("data:image/png"));
        assert!(!output.contains("data:audio/wav"));
        assert!(output.contains(NON_TEXT_MARKER));
    }

    #[test]
    fn emitted_media_without_text_is_represented_without_its_url() {
        let media = ToolOutputBody::Content(vec![ToolOutputContent::InputImage {
            image_url: "data:image/png;base64,secret".to_owned(),
            detail: ImageDetail::High,
        }]);

        let ToolOutputBody::Text(output) =
            bounded_emitted_output([&media]).expect("output should be emitted")
        else {
            panic!("emitted output should be text");
        };
        assert_eq!(output, NON_TEXT_MARKER);
    }
}
