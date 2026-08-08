use std::{io, sync::Arc};

use nanocodex::{
    Tool, ToolTurnBehavior, Tools,
    agent::AgentHandle,
    oai::tools::ToolDefinition,
    tools::{
        ToolContext, ToolInput, ToolOutput, ToolResult, ToolsBuildError, contract::async_trait,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{SpineRuntime, SpineRuntimeError, SpineTerminalTransition, TerminalControl};

/// Adds the synchronous Spine control tools to one agent-local tool set.
///
/// The same factory must be used for every fork so nested continuations keep
/// one shared reducer while each session receives a handle to itself.
pub fn with_spine_tools(
    tools: Tools,
    agent: AgentHandle,
    runtime: Arc<SpineRuntime>,
) -> Result<Tools, ToolsBuildError> {
    tools
        .into_builder()
        .tool(SpineOpen {
            agent,
            runtime: Arc::clone(&runtime),
        })
        .tool(SpineClose {
            runtime: Arc::clone(&runtime),
        })
        .tool(SpineNext { runtime })
        .build()
}

struct SpineOpen {
    agent: AgentHandle,
    runtime: Arc<SpineRuntime>,
}

struct SpineClose {
    runtime: Arc<SpineRuntime>,
}

struct SpineNext {
    runtime: Arc<SpineRuntime>,
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

#[derive(Serialize)]
struct OpenResult {
    closed_node: String,
    memory: String,
}

#[async_trait]
impl Tool for SpineOpen {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "spine__open",
            "Open one synchronous semantic work scope. This blocks until that scope returns a compact handoff. Use it only for genuine focused work, never in a parallel batch.",
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
        ToolTurnBehavior::EmitOutputOnSuccess
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let OpenArgs { summary } = input.decode_json()?;
        let mut transaction = SpineOpenTransaction::begin(Arc::clone(&self.runtime))?;
        self.runtime.open(context.call_id(), &summary)?;
        match run_continuation(&self.agent, Arc::clone(&self.runtime), summary).await {
            Ok(result) => {
                transaction.commit();
                let model_output = format!("<spine_memory>\n{}\n</spine_memory>", result.memory);
                Ok(ToolOutput::text(model_output).with_code_mode_value(json!({
                    "closed_node": result.closed_node,
                    "memory": result.memory,
                })))
            }
            Err(error) => {
                transaction.rollback()?;
                Err(error)
            }
        }
    }
}

struct SpineOpenTransaction {
    runtime: Arc<SpineRuntime>,
    checkpoint: Option<super::SpineRuntimeCheckpoint>,
}

impl SpineOpenTransaction {
    fn begin(runtime: Arc<SpineRuntime>) -> Result<Self, SpineRuntimeError> {
        let checkpoint = runtime.checkpoint()?;
        Ok(Self {
            runtime,
            checkpoint: Some(checkpoint),
        })
    }

    fn commit(&mut self) {
        self.checkpoint = None;
    }

    fn rollback(&mut self) -> Result<(), SpineRuntimeError> {
        if let Some(checkpoint) = self.checkpoint.take() {
            self.runtime.restore(checkpoint)?;
        }
        Ok(())
    }
}

impl Drop for SpineOpenTransaction {
    fn drop(&mut self) {
        let _ = self.rollback();
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

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let CloseArgs { memory } = input.decode_json()?;
        self.runtime.validate_memory(&memory)?;
        self.runtime.ensure_live_task()?;
        Ok(ToolOutput::json(&Accepted { accepted: true })
            .with_metadata(TerminalControl::Close { memory }))
    }
}

#[async_trait]
impl Tool for SpineNext {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "spine__next",
            "Finish the current Spine scope and begin a sibling scope. The enclosing Code Mode cell must finish successfully for this transition to commit.",
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

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let NextArgs { summary, memory } = input.decode_json()?;
        self.runtime.validate_summary(&summary)?;
        self.runtime.validate_memory(&memory)?;
        self.runtime.ensure_live_task()?;
        Ok(ToolOutput::json(&Accepted { accepted: true })
            .with_metadata(TerminalControl::Next { summary, memory }))
    }
}

async fn run_continuation(
    parent: &AgentHandle,
    runtime: Arc<SpineRuntime>,
    mut summary: String,
) -> Result<OpenResult, Box<dyn std::error::Error + Send + Sync>> {
    let mut prior_memory = None;
    loop {
        let context = runtime.continuation_context(&summary, prior_memory.as_deref())?;
        let (child, events) = parent.fork().await?;
        drop(events);
        let result = match child.prompt(context.render()).await {
            Ok(turn) => turn.result().await,
            Err(error) => Err(error),
        };
        let shutdown_result = child.shutdown().await;
        let result = result?;
        shutdown_result?;

        let transition = match result.completion() {
            nanocodex::TurnCompletion::TerminalTool { receipt } => {
                runtime.accept_terminal_receipt(receipt)?
            }
            nanocodex::TurnCompletion::Message { .. } => {
                return Err(io::Error::other(
                    "a Spine continuation ended with a message; call spine__close or spine__next instead",
                )
                .into());
            }
            _ => {
                return Err(io::Error::other("unsupported Spine continuation completion").into());
            }
        };
        match transition {
            SpineTerminalTransition::Closed { handoff, memory } => {
                return Ok(OpenResult {
                    closed_node: handoff.closed_node.to_string(),
                    memory,
                });
            }
            SpineTerminalTransition::Next {
                summary: next_summary,
                memory,
                ..
            } => {
                summary = next_summary;
                prior_memory = Some(memory);
            }
        }
    }
}
