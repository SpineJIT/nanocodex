use std::collections::HashSet;

use crate::{
    ContentItem, FunctionOutputBody, FunctionOutputContent, MessageRole, ResponseItem,
    ResponseItemId, Usage, responses::ResponseHistory,
};

use crate::compaction;

const TOOL_OUTPUT_TOKEN_LIMIT: usize = 12_000;

/// Typed model-visible transcript. The common prompt path shares its backing
/// allocation; prompt-only repairs allocate only for incomplete call pairs.
#[derive(Clone)]
pub struct ContextManager {
    items: ResponseHistory,
    last_token_usage: Option<Usage>,
    calls: CallIds,
}

#[derive(Clone, Default)]
struct CallIds {
    function_calls: HashSet<Box<str>>,
    function_outputs: HashSet<Box<str>>,
    custom_calls: HashSet<Box<str>>,
    custom_outputs: HashSet<Box<str>>,
    tool_search_calls: HashSet<Box<str>>,
    tool_search_outputs: HashSet<Box<str>>,
}

impl ContextManager {
    #[must_use]
    pub fn new(items: Vec<ResponseItem>) -> Self {
        let mut context = Self {
            items: ResponseHistory::default(),
            last_token_usage: None,
            calls: CallIds::default(),
        };
        context.record_items(items);
        context
    }

    #[must_use]
    pub fn flattened_items(&self) -> Vec<ResponseItem> {
        self.items.iter().cloned().collect()
    }

    #[must_use]
    pub fn shared_items(&self) -> ResponseHistory {
        self.items.clone()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ResponseItem> {
        self.items.iter()
    }

    pub fn record_items(&mut self, items: impl IntoIterator<Item = ResponseItem>) {
        for mut item in items
            .into_iter()
            .filter(is_api_item)
            .map(truncate_tool_output)
        {
            assign_missing_response_item_id(&mut item);
            self.calls.track(&item);
            self.items.push(item);
        }
    }

    pub fn commit_tail(&mut self) {
        self.items.commit_tail();
        if self.calls.is_balanced() {
            self.calls.clear();
        }
    }

    pub fn replace_and_recompute(&mut self, mut items: Vec<ResponseItem>, prefix: &[ResponseItem]) {
        assign_missing_response_item_ids(&mut items);
        self.items.replace(items);
        let total_tokens = prefix
            .iter()
            .chain(self.items.iter())
            .map(compaction::estimate_item_tokens)
            .fold(0, u64::saturating_add);
        self.last_token_usage = Some(Usage {
            total_tokens,
            ..Usage::default()
        });
        self.calls.clear();
        for item in self.items.iter() {
            self.calls.track(item);
        }
    }

    pub fn update_token_info(&mut self, usage: Option<&Usage>) {
        if let Some(usage) = usage {
            self.last_token_usage = Some(usage.clone());
        }
    }

    #[must_use]
    pub fn active_context_tokens(&self, server_reasoning_included: bool) -> u64 {
        let reported = self
            .last_token_usage
            .as_ref()
            .map_or(0, |usage| usage.total_tokens);
        let local_tail = self.items_after_last_model_generated_tokens();
        if server_reasoning_included {
            reported.saturating_add(local_tail)
        } else {
            reported
                .saturating_add(self.non_last_reasoning_tokens())
                .saturating_add(local_tail)
        }
    }

    /// Returns a shared prompt snapshot, allocating a repaired copy only for
    /// missing call outputs or orphan outputs.
    #[must_use]
    pub fn prompt_items(&self) -> ResponseHistory {
        let needs_repair = !self.calls.is_balanced();
        if !needs_repair {
            return self.items.clone();
        }

        let mut repaired = Vec::with_capacity(self.items.len() + 2);
        for item in &self.items {
            match item {
                ResponseItem::FunctionCall { call_id, .. }
                | ResponseItem::LocalShellCall {
                    call_id: Some(call_id),
                    ..
                } => {
                    repaired.push(item.clone());
                    if !self.calls.function_outputs.contains(call_id.as_ref()) {
                        repaired.push(ResponseItem::function_call_output(
                            call_id.to_string(),
                            FunctionOutputBody::Text("aborted".into()),
                        ));
                    }
                }
                ResponseItem::CustomToolCall { call_id, .. } => {
                    repaired.push(item.clone());
                    if !self.calls.custom_outputs.contains(call_id.as_ref()) {
                        repaired.push(ResponseItem::custom_tool_output(
                            call_id.to_string(),
                            None,
                            FunctionOutputBody::Text("aborted".into()),
                        ));
                    }
                }
                ResponseItem::FunctionCallOutput { call_id, .. }
                    if !self.calls.function_calls.contains(call_id.as_ref()) => {}
                ResponseItem::CustomToolCallOutput { call_id, .. }
                    if !self.calls.custom_calls.contains(call_id.as_ref()) => {}
                ResponseItem::ToolSearchCall {
                    call_id: Some(call_id),
                    ..
                } => {
                    repaired.push(item.clone());
                    if !self.calls.tool_search_outputs.contains(call_id.as_ref()) {
                        repaired.push(ResponseItem::ToolSearchOutput {
                            id: None,
                            call_id: Some(call_id.clone()),
                            status: "completed".into(),
                            execution: "client".into(),
                            tools: Vec::new(),
                            internal_chat_message_metadata_passthrough: None,
                        });
                    }
                }
                ResponseItem::ToolSearchOutput {
                    call_id: Some(call_id),
                    execution,
                    ..
                } if execution.as_ref() != "server"
                    && !self.calls.tool_search_calls.contains(call_id.as_ref()) => {}
                _ => repaired.push(item.clone()),
            }
        }
        ResponseHistory::new(repaired)
    }

    fn items_after_last_model_generated_tokens(&self) -> u64 {
        let mut tokens = 0_u64;
        for item in &self.items {
            if is_model_generated_item(item) {
                tokens = 0;
            } else {
                tokens = tokens.saturating_add(compaction::estimate_item_tokens(item));
            }
        }
        tokens
    }

    fn non_last_reasoning_tokens(&self) -> u64 {
        let mut reasoning = 0_u64;
        let mut before_last_user = None;
        for item in &self.items {
            if is_user_turn_boundary(item) {
                before_last_user = Some(reasoning);
            }
            if matches!(
                item,
                ResponseItem::Reasoning {
                    encrypted_content: Some(_),
                    ..
                }
            ) {
                reasoning = reasoning.saturating_add(compaction::estimate_item_tokens(item));
            }
        }
        before_last_user.unwrap_or_default()
    }
}

impl CallIds {
    fn is_balanced(&self) -> bool {
        self.function_calls == self.function_outputs
            && self.custom_calls == self.custom_outputs
            && self.tool_search_calls == self.tool_search_outputs
    }

    fn clear(&mut self) {
        self.function_calls.clear();
        self.function_outputs.clear();
        self.custom_calls.clear();
        self.custom_outputs.clear();
        self.tool_search_calls.clear();
        self.tool_search_outputs.clear();
    }

    fn track(&mut self, item: &ResponseItem) {
        match item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => {
                self.function_calls.insert(call_id.clone());
            }
            ResponseItem::FunctionCallOutput { call_id, .. } => {
                self.function_outputs.insert(call_id.clone());
            }
            ResponseItem::CustomToolCall { call_id, .. } => {
                self.custom_calls.insert(call_id.clone());
            }
            ResponseItem::CustomToolCallOutput { call_id, .. } => {
                self.custom_outputs.insert(call_id.clone());
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                ..
            } => {
                self.tool_search_calls.insert(call_id.clone());
            }
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } => {
                self.tool_search_outputs.insert(call_id.clone());
            }
            _ => {}
        }
    }
}

pub fn assign_missing_response_item_ids(items: &mut [ResponseItem]) {
    for item in items {
        assign_missing_response_item_id(item);
    }
}

pub fn assign_missing_response_item_id(item: &mut ResponseItem) {
    if item.id().is_some_and(|id| !id.is_empty()) {
        return;
    }
    let Some(prefix) = item.id_prefix() else {
        return;
    };
    item.set_id(Some(new_response_item_id(prefix)));
}

fn new_response_item_id(prefix: &str) -> ResponseItemId {
    ResponseItemId::with_suffix(prefix, uuid::Uuid::now_v7())
}

#[must_use]
pub fn has_well_formed_tool_calls(items: &[ResponseItem]) -> bool {
    let mut function_calls = HashSet::new();
    let mut function_outputs = HashSet::new();
    let mut custom_calls = HashSet::new();
    let mut custom_outputs = HashSet::new();
    let mut search_calls = HashSet::new();
    let mut search_outputs = HashSet::new();
    for item in items {
        let valid = match item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => function_calls.insert(call_id.as_ref()),
            ResponseItem::FunctionCallOutput { call_id, .. } => {
                function_calls.contains(call_id.as_ref())
                    && function_outputs.insert(call_id.as_ref())
            }
            ResponseItem::CustomToolCall { call_id, .. } => custom_calls.insert(call_id.as_ref()),
            ResponseItem::CustomToolCallOutput { call_id, .. } => {
                custom_calls.contains(call_id.as_ref()) && custom_outputs.insert(call_id.as_ref())
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                ..
            } => search_calls.insert(call_id.as_ref()),
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } => search_calls.contains(call_id.as_ref()) && search_outputs.insert(call_id.as_ref()),
            _ => true,
        };
        if !valid {
            return false;
        }
    }
    function_calls == function_outputs
        && custom_calls == custom_outputs
        && search_calls == search_outputs
}

const fn is_model_generated_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Message {
            role: MessageRole::Assistant,
            ..
        } | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. }
    )
}

fn is_user_turn_boundary(item: &ResponseItem) -> bool {
    item.is_user_message() && !is_contextual_user_message(item)
}

#[must_use]
pub fn is_contextual_user_message(item: &ResponseItem) -> bool {
    let ResponseItem::Message { content, .. } = item else {
        return false;
    };
    content
        .iter()
        .filter_map(|content| {
            let ContentItem::InputText { text } = content else {
                return None;
            };
            Some(text.as_ref())
        })
        .any(|text| {
            matches_marked_text("# AGENTS.md instructions", "</INSTRUCTIONS>", text)
                || matches_marked_text("<environment_context>", "</environment_context>", text)
                || matches_marked_text("<turn_aborted>", "</turn_aborted>", text)
        })
}

fn matches_marked_text(start: &str, end: &str, text: &str) -> bool {
    let text = text.trim();
    text.get(..start.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(start))
        && text
            .get(text.len().saturating_sub(end.len())..)
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(end))
}

const fn is_api_item(item: &ResponseItem) -> bool {
    !matches!(
        item,
        ResponseItem::CompactionTrigger {} | ResponseItem::Other(_)
    )
}

fn truncate_tool_output(mut item: ResponseItem) -> ResponseItem {
    let (ResponseItem::FunctionCallOutput { output, .. }
    | ResponseItem::CustomToolCallOutput { output, .. }) = &mut item
    else {
        return item;
    };
    match output {
        FunctionOutputBody::Text(text) => {
            *text = compaction::truncate_middle_with_token_budget(text, TOOL_OUTPUT_TOKEN_LIMIT)
                .into_boxed_str();
        }
        FunctionOutputBody::Content(content) => {
            truncate_output_content(content, TOOL_OUTPUT_TOKEN_LIMIT);
        }
    }
    item
}

fn truncate_output_content(items: &mut Vec<FunctionOutputContent>, token_limit: usize) {
    let mut remaining = token_limit;
    let mut omitted_text_items = 0usize;
    let mut output = Vec::with_capacity(items.len());
    for mut item in std::mem::take(items) {
        match &mut item {
            FunctionOutputContent::InputText { text } => {
                if remaining == 0 {
                    omitted_text_items += 1;
                    continue;
                }
                let tokens = text.len().div_ceil(4);
                if tokens <= remaining {
                    remaining -= tokens;
                    output.push(item);
                } else {
                    *text = compaction::truncate_middle_with_token_budget(text, remaining)
                        .into_boxed_str();
                    if text.is_empty() {
                        omitted_text_items += 1;
                    } else {
                        output.push(item);
                    }
                    remaining = 0;
                }
            }
            FunctionOutputContent::InputImage { .. }
            | FunctionOutputContent::EncryptedContent { .. } => output.push(item),
            FunctionOutputContent::InputAudio { .. } => {}
        }
    }
    if omitted_text_items > 0 {
        output.push(FunctionOutputContent::InputText {
            text: format!("[omitted {omitted_text_items} text items ...]").into_boxed_str(),
        });
    }
    *items = output;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_prompt_reuses_the_history_without_repair() {
        let context = ContextManager::new(vec![message("hello")]);
        let prompt = context.prompt_items();
        assert_eq!(prompt.len(), 1);
    }

    #[test]
    fn history_assigns_ids_once_and_preserves_them_across_checkpoints() {
        let mut context = ContextManager::new(vec![message("hello")]);
        let id = context
            .flattened_items()
            .into_iter()
            .next()
            .and_then(|item| item.id().cloned())
            .expect("history item should have an ID");
        assert!(id.as_str().starts_with("msg_"));

        context.commit_tail();
        let checkpoint = context.shared_items();
        assert_eq!(
            checkpoint.iter().next().and_then(ResponseItem::id),
            Some(&id)
        );
    }

    #[test]
    fn prompt_repairs_do_not_mutate_raw_history() {
        let call: ResponseItem = serde_json::from_str(
            r#"{"type":"custom_tool_call","call_id":"missing","name":"exec","input":"code"}"#,
        )
        .unwrap();
        let orphan = ResponseItem::custom_tool_output(
            "orphan".to_owned(),
            None,
            FunctionOutputBody::Text("unused".into()),
        );
        let context = ContextManager::new(vec![call, orphan]);
        let prompt = context.prompt_items();
        let prompt: Vec<_> = prompt.iter().collect();
        assert_eq!(context.flattened_items().len(), 2);
        assert_eq!(prompt.len(), 2);
        assert!(matches!(
            &prompt[1],
            ResponseItem::CustomToolCallOutput { call_id, output: FunctionOutputBody::Text(text), .. }
                if call_id.as_ref() == "missing" && text.as_ref() == "aborted"
        ));
    }

    #[test]
    fn history_truncates_tool_text_but_preserves_images() {
        let context = ContextManager::new(vec![ResponseItem::custom_tool_output(
            "call".to_owned(),
            None,
            FunctionOutputBody::Content(vec![
                FunctionOutputContent::InputText {
                    text: "x".repeat(48_004).into_boxed_str(),
                },
                FunctionOutputContent::InputImage {
                    image_url: "data:image/png;base64,a".into(),
                    detail: None,
                },
                FunctionOutputContent::InputText {
                    text: "omitted".into(),
                },
            ]),
        )]);
        let history = context.flattened_items();
        let ResponseItem::CustomToolCallOutput {
            output: FunctionOutputBody::Content(output),
            ..
        } = &history[0]
        else {
            panic!("expected content output")
        };
        assert!(
            matches!(&output[0], FunctionOutputContent::InputText { text } if text.contains("tokens truncated"))
        );
        assert!(matches!(
            &output[1],
            FunctionOutputContent::InputImage { .. }
        ));
        assert!(
            matches!(&output[2], FunctionOutputContent::InputText { text } if text.as_ref() == "[omitted 1 text items ...]")
        );
    }

    #[test]
    fn contextual_messages_require_start_and_end_markers() {
        assert!(is_contextual_user_message(&message(
            "  # agents.md instructions\n\n<INSTRUCTIONS>\nnew\n</instructions>\n"
        )));
        assert!(!is_contextual_user_message(&message(
            "# AGENTS.md instructions are useful"
        )));
        assert!(is_contextual_user_message(&message(
            "<turn_aborted>\ninterrupted\n</turn_aborted>"
        )));
    }

    fn message(text: &str) -> ResponseItem {
        ResponseItem::message(
            MessageRole::User,
            [ContentItem::InputText { text: text.into() }],
        )
    }
}
