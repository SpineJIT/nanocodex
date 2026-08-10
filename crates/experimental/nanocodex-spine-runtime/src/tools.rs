use std::sync::Arc;

use nanocodex::{
    Tool, ToolTurnBehavior, Tools,
    oai::tools::ToolDefinition,
    tools::{
        ToolContext, ToolInput, ToolOutput, ToolResult, ToolsBuildError, contract::async_trait,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{SpineIntentRequest, SpineIntentSink, SpineTerminalControl};

/// Adds terminal Spine controls backed by the coordinator-owned intent sink.
pub fn with_spine_tools(
    tools: Tools,
    intent_sink: Arc<dyn SpineIntentSink>,
) -> Result<Tools, ToolsBuildError> {
    tools
        .into_builder()
        .tool(SpineOpen {
            intent_sink: Arc::clone(&intent_sink),
        })
        .tool(SpineClose {
            intent_sink: Arc::clone(&intent_sink),
        })
        .tool(SpineNext { intent_sink })
        .build()
}

struct SpineOpen {
    intent_sink: Arc<dyn SpineIntentSink>,
}

struct SpineClose {
    intent_sink: Arc<dyn SpineIntentSink>,
}

struct SpineNext {
    intent_sink: Arc<dyn SpineIntentSink>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenArgs {
    summary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseArgs {
    memory: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NextArgs {
    summary: String,
    memory: String,
}

#[derive(Serialize)]
struct Accepted {
    accepted: bool,
}

#[async_trait]
impl Tool for SpineOpen {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "spine__open",
            "Finish this turn, park the current Spine scope, and activate one focused child scope. Use it only for genuine focused work, never in a parallel batch.",
            json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "A concise, actionable goal for the child scope."
                    }
                },
                "required": ["summary"],
                "additionalProperties": false
            }),
        )
    }

    fn turn_behavior(&self) -> ToolTurnBehavior {
        ToolTurnBehavior::FinishTurnOnSuccess
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let OpenArgs { summary } = input.decode_json()?;
        let control = SpineTerminalControl::Open { summary };
        self.intent_sink
            .prepare(SpineIntentRequest::new(
                context.session_id(),
                context.call_id(),
                control.clone(),
            ))
            .await?;
        Ok(ToolOutput::json(&Accepted { accepted: true }).with_metadata(control))
    }
}

#[async_trait]
impl Tool for SpineClose {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "spine__close",
            "Finish the current Spine scope with compact memory. The enclosing Code Mode cell must finish successfully for this transition to commit.",
            json!({
                "type": "object",
                "properties": {
                    "memory": {
                        "type": "string",
                        "description": "Compact continuation state for the frozen parent."
                    }
                },
                "required": ["memory"],
                "additionalProperties": false
            }),
        )
    }

    fn turn_behavior(&self) -> ToolTurnBehavior {
        ToolTurnBehavior::FinishTurnOnSuccess
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let CloseArgs { memory } = input.decode_json()?;
        let control = SpineTerminalControl::Close { memory };
        self.intent_sink
            .prepare(SpineIntentRequest::new(
                context.session_id(),
                context.call_id(),
                control.clone(),
            ))
            .await?;
        Ok(ToolOutput::json(&Accepted { accepted: true }).with_metadata(control))
    }
}

#[async_trait]
impl Tool for SpineNext {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "spine__next",
            "Finish the current Spine scope and activate a sibling from the frozen parent. The enclosing Code Mode cell must finish successfully for this transition to commit.",
            json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "A concise, actionable goal for the sibling scope."
                    },
                    "memory": {
                        "type": "string",
                        "description": "Compact continuation state from the closed sibling."
                    }
                },
                "required": ["summary", "memory"],
                "additionalProperties": false
            }),
        )
    }

    fn turn_behavior(&self) -> ToolTurnBehavior {
        ToolTurnBehavior::FinishTurnOnSuccess
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let NextArgs { summary, memory } = input.decode_json()?;
        let control = SpineTerminalControl::Next { summary, memory };
        self.intent_sink
            .prepare(SpineIntentRequest::new(
                context.session_id(),
                context.call_id(),
                control.clone(),
            ))
            .await?;
        Ok(ToolOutput::json(&Accepted { accepted: true }).with_metadata(control))
    }
}
