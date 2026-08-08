use crate::contract::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolOutputBody, ToolOutputContent};

const MAX_EMITTED_OUTPUT_BYTES: usize = 1_000;
// One UTF-8 byte is a conservative upper bound on one text token, unlike the
// ordinary four-bytes-per-token display estimate used for non-handoff output.
const MAX_MODEL_VISIBLE_OUTPUT_BYTES: usize = DEFAULT_TOOL_OUTPUT_TOKENS;
const TRUNCATION_MARKER: &str = "\n[emitted output truncated]\n";
const NORMAL_OUTPUT_TRUNCATION_MARKER: &str = "\n[Code Mode output truncated]\n";
const NON_TEXT_MARKER: &str = "[non-text emitted output omitted]";

pub(crate) fn bounded_emitted_output<'a>(
    outputs: impl IntoIterator<Item = &'a ToolOutputBody>,
) -> Option<ToolOutputBody> {
    let mut text = BoundedText::new(MAX_EMITTED_OUTPUT_BYTES, TRUNCATION_MARKER);
    for output in outputs {
        if !text.is_empty() {
            text.push("\n");
        }
        append_output_text(&mut text, output);
    }
    text.finish().map(ToolOutputBody::Text)
}

pub(crate) fn append_bounded_output(output: &mut ToolOutputBody, emitted: ToolOutputBody) {
    let Some(ToolOutputBody::Text(emitted)) = bounded_emitted_output([&emitted]) else {
        return;
    };
    let normal_budget = MAX_MODEL_VISIBLE_OUTPUT_BYTES
        .saturating_sub(emitted.len())
        .saturating_sub(1);
    let normal = bounded_output_text(output, normal_budget);
    let separator = if normal.is_empty() { "" } else { "\n" };
    *output = ToolOutputBody::Text(format!("{normal}{separator}{emitted}"));
}

struct BoundedText {
    text: String,
    limit: usize,
    truncation_marker: &'static str,
    truncated: bool,
}

impl BoundedText {
    fn new(limit: usize, truncation_marker: &'static str) -> Self {
        Self {
            text: String::new(),
            limit,
            truncation_marker,
            truncated: false,
        }
    }

    const fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn push(&mut self, value: &str) {
        if self.truncated || value.is_empty() {
            return;
        }
        let remaining = self.limit.saturating_sub(self.text.len());
        if value.len() <= remaining {
            self.text.push_str(value);
            return;
        }
        let prefix_limit = self.limit.saturating_sub(self.truncation_marker.len());
        self.text
            .truncate(floor_char_boundary(&self.text, prefix_limit));
        let end = floor_char_boundary(value, prefix_limit.saturating_sub(self.text.len()));
        self.text.push_str(&value[..end]);
        self.truncated = true;
    }

    fn finish(mut self) -> Option<String> {
        if self.truncated {
            let marker_end = floor_char_boundary(
                self.truncation_marker,
                self.limit.saturating_sub(self.text.len()),
            );
            self.text.push_str(&self.truncation_marker[..marker_end]);
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

fn bounded_output_text(output: &ToolOutputBody, limit: usize) -> String {
    let mut text = BoundedText::new(limit, NORMAL_OUTPUT_TRUNCATION_MARKER);
    append_output_text(&mut text, output);
    text.finish().unwrap_or_default()
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
        MAX_EMITTED_OUTPUT_BYTES, MAX_MODEL_VISIBLE_OUTPUT_BYTES, NON_TEXT_MARKER,
        NORMAL_OUTPUT_TRUNCATION_MARKER, TRUNCATION_MARKER, append_bounded_output,
        bounded_emitted_output,
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

    #[test]
    fn final_emitting_output_is_text_only_and_fits_the_safe_byte_cap() {
        let emitted = ToolOutputBody::Text("<spine_memory>handoff</spine_memory>".to_owned());
        let mut normal = ToolOutputBody::Content(vec![
            ToolOutputContent::InputImage {
                image_url: format!("data:image/png;base64,{}", "x".repeat(20_000)),
                detail: ImageDetail::High,
            },
            ToolOutputContent::InputAudio {
                audio_url: format!("data:audio/wav;base64,{}", "y".repeat(20_000)),
            },
            ToolOutputContent::InputText {
                text: "界".repeat(20_000),
            },
        ]);

        append_bounded_output(&mut normal, emitted);

        let ToolOutputBody::Text(output) = normal else {
            panic!("emitting output should be normalized to text");
        };
        assert!(output.len() <= MAX_MODEL_VISIBLE_OUTPUT_BYTES);
        assert!(output.is_char_boundary(output.len()));
        assert!(output.contains(NON_TEXT_MARKER));
        assert!(output.contains(NORMAL_OUTPUT_TRUNCATION_MARKER));
        assert!(output.ends_with("<spine_memory>handoff</spine_memory>"));
        assert!(!output.contains("data:image/png"));
        assert!(!output.contains("data:audio/wav"));
    }
}
