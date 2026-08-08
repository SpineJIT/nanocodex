use super::*;

use nanocodex_agent::TurnCompletion;
use nanocodex_tools::{
    TerminalToolReceipt, TerminalToolReceiptError, Tool, ToolContext, ToolDefinition, ToolExposure,
    ToolInput, ToolOutput, ToolResult, ToolTurnBehavior, contract::async_trait,
};

struct FinishTurn;

struct OversizedFinishTurn;

struct FailingTool;

#[async_trait]
impl Tool for FinishTurn {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "finish_turn",
            "Finishes the enclosing turn after the complete Code Mode cell succeeds.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    fn turn_behavior(&self) -> ToolTurnBehavior {
        ToolTurnBehavior::FinishTurnOnSuccess
    }

    async fn execute(&self, _input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        Ok(ToolOutput::text("terminal result"))
    }
}

#[async_trait]
impl Tool for OversizedFinishTurn {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "oversized_finish_turn",
            "Returns more data than a terminal receipt can retain.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    fn turn_behavior(&self) -> ToolTurnBehavior {
        ToolTurnBehavior::FinishTurnOnSuccess
    }

    async fn execute(&self, _input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        Ok(ToolOutput::text(
            "x".repeat(TerminalToolReceipt::MAX_OUTPUT_BYTES * 2),
        ))
    }
}

#[async_trait]
impl Tool for FailingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "failing_tool",
            "Returns a model-visible failure.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, _input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        Ok(ToolOutput::error("intentional failure"))
    }
}

#[tokio::test]
async fn successful_terminal_code_mode_cell_stops_before_a_follow_on_model_request() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let warmup = next_json(&mut socket).await?;
        assert_eq!(warmup["parallel_tool_calls"], false);
        send_warmup(&mut socket, "resp-warmup").await?;

        let _generation = next_json(&mut socket).await?;
        send_json(
            &mut socket,
            completed_response(
                "resp-terminal",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "const result = await tools.finish_turn({}); text(result);"
                })],
            ),
        )
        .await?;

        match timeout(
            std::time::Duration::from_millis(250),
            next_json(&mut socket),
        )
        .await
        {
            Err(_) => Ok(()),
            Ok(Ok(request)) => Err(eyre!(
                "terminal Code Mode cell unexpectedly requested a continuation: {request}"
            )),
            Ok(Err(error)) => Err(error),
        }
    });

    let workspace = temporary_workspace("terminal-code-mode-turn")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(&endpoint)
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool(FinishTurn)
        .build()?;
    let (agent, mut events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .tools(tools)
        .build()?;

    let result = agent
        .prompt("Finish with the application tool.")
        .await?
        .result()
        .await?;
    match result.completion() {
        TurnCompletion::TerminalTool { receipt } => {
            assert_eq!(receipt.call_id(), "call-exec/code-1");
            assert_eq!(receipt.tool_name(), "finish_turn");
        }
        TurnCompletion::Message { .. } => panic!("terminal tool completion returned a message"),
        _ => panic!("unknown terminal tool completion"),
    }
    assert_eq!(result.final_message(), None);

    let terminal = loop {
        let event = events
            .recv()
            .await
            .ok_or_else(|| eyre!("agent event stream closed before run completion"))?;
        if event.kind == AgentEventKind::RunCompleted {
            break event.decode_payload::<Value>()?;
        }
    };
    assert_eq!(terminal["completion"]["type"], "terminal_tool");
    assert_eq!(terminal["completion"]["tool_name"], "finish_turn");

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    agent.shutdown().await?;
    drop((agent, events));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn terminal_code_mode_follow_on_replays_the_nested_tool_result() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let _warmup = next_json(&mut socket).await?;
        send_warmup(&mut socket, "resp-warmup").await?;

        let _generation = next_json(&mut socket).await?;
        send_json(
            &mut socket,
            completed_response(
                "resp-terminal",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "await tools.finish_turn({});"
                })],
            ),
        )
        .await?;

        let follow_on = next_json(&mut socket).await?;
        assert!(follow_on.get("previous_response_id").is_none());
        let replay = follow_on.to_string();
        assert!(replay.contains("call-exec/code-1"));
        assert!(replay.contains("terminal result"));
        assert!(replay.contains("Continue after the terminal result."));
        send_final(&mut socket, "resp-follow-on").await
    });

    let workspace = temporary_workspace("terminal-code-mode-follow-on")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(&endpoint)
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool(FinishTurn)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .tools(tools)
        .build()?;

    let terminal = agent
        .prompt("Finish with the application tool.")
        .await?
        .result()
        .await?;
    assert!(matches!(
        terminal.completion(),
        TurnCompletion::TerminalTool { .. }
    ));

    let follow_on = agent
        .prompt("Continue after the terminal result.")
        .await?
        .result()
        .await?;
    assert!(matches!(
        follow_on.completion(),
        TurnCompletion::Message { final_message } if final_message == "done"
    ));

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    agent.shutdown().await?;
    drop((agent, events));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn terminal_code_mode_wait_follow_on_replays_a_valid_tool_result() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let _warmup = next_json(&mut socket).await?;
        send_warmup(&mut socket, "resp-warmup").await?;

        let _generation = next_json(&mut socket).await?;
        send_json(
            &mut socket,
            completed_response(
                "resp-exec",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "await yield_control(); await tools.finish_turn({});"
                })],
            ),
        )
        .await?;

        let yielded = next_json(&mut socket).await?;
        assert_eq!(yielded["previous_response_id"], "resp-exec");
        send_json(
            &mut socket,
            completed_response(
                "resp-wait",
                &[json!({
                    "type": "function_call",
                    "call_id": "call-wait",
                    "name": "wait",
                    "arguments": "{\"cell_id\":\"1\",\"yield_time_ms\":30000}"
                })],
            ),
        )
        .await?;

        let follow_on = next_json(&mut socket).await?;
        assert!(follow_on.get("previous_response_id").is_none());
        let replay = follow_on.to_string();
        assert!(replay.contains("call-exec/code-1"));
        assert!(replay.contains("terminal result"));
        assert!(replay.contains("Continue after the terminal result."));
        let history = follow_on["input"]
            .as_array()
            .ok_or_else(|| eyre!("full replay did not contain input items"))?;
        assert!(history.iter().any(|item| {
            item["type"] == "function_call_output" && item["call_id"] == "call-wait"
        }));
        assert!(!history.iter().any(|item| {
            item["type"] == "custom_tool_call_output" && item["call_id"] == "call-wait"
        }));
        send_final(&mut socket, "resp-follow-on").await
    });

    let workspace = temporary_workspace("terminal-code-mode-wait-follow-on")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(&endpoint)
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool(FinishTurn)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .tools(tools)
        .build()?;

    let terminal = agent
        .prompt("Finish with the application tool.")
        .await?
        .result()
        .await?;
    assert!(matches!(
        terminal.completion(),
        TurnCompletion::TerminalTool { .. }
    ));

    let follow_on = agent
        .prompt("Continue after the terminal result.")
        .await?
        .result()
        .await?;
    assert!(matches!(
        follow_on.completion(),
        TurnCompletion::Message { final_message } if final_message == "done"
    ));

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    agent.shutdown().await?;
    drop((agent, events));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn successful_direct_terminal_tool_stops_before_a_follow_on_model_request() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let warmup = next_json(&mut socket).await?;
        assert_eq!(warmup["parallel_tool_calls"], false);
        send_warmup(&mut socket, "resp-warmup").await?;

        let _generation = next_json(&mut socket).await?;
        send_json(
            &mut socket,
            completed_response(
                "resp-terminal",
                &[json!({
                    "type": "function_call",
                    "call_id": "call-finish",
                    "name": "finish_turn",
                    "arguments": "{}"
                })],
            ),
        )
        .await?;

        match timeout(
            std::time::Duration::from_millis(250),
            next_json(&mut socket),
        )
        .await
        {
            Err(_) => Ok(()),
            Ok(Ok(request)) => Err(eyre!(
                "terminal direct tool unexpectedly requested a continuation: {request}"
            )),
            Ok(Err(error)) => Err(error),
        }
    });

    let workspace = temporary_workspace("terminal-direct-tool-turn")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(&endpoint)
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool_with_exposure(FinishTurn, ToolExposure::DirectOnly)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .tools(tools)
        .build()?;

    let result = agent
        .prompt("Finish with the application tool.")
        .await?
        .result()
        .await?;
    match result.completion() {
        TurnCompletion::TerminalTool { receipt } => {
            assert_eq!(receipt.call_id(), "call-finish");
            assert_eq!(receipt.tool_name(), "finish_turn");
        }
        TurnCompletion::Message { .. } => {
            panic!("terminal direct tool completion returned a message")
        }
        _ => panic!("unknown terminal tool completion"),
    }
    assert_eq!(result.final_message(), None);

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    agent.shutdown().await?;
    drop((agent, events));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn terminal_tool_follow_on_replays_the_committed_tool_result() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let _warmup = next_json(&mut socket).await?;
        send_warmup(&mut socket, "resp-warmup").await?;

        let _generation = next_json(&mut socket).await?;
        send_json(
            &mut socket,
            completed_response(
                "resp-terminal",
                &[json!({
                    "type": "function_call",
                    "call_id": "call-finish",
                    "name": "finish_turn",
                    "arguments": "{}"
                })],
            ),
        )
        .await?;

        let follow_on = next_json(&mut socket).await?;
        assert!(follow_on.get("previous_response_id").is_none());
        let replay = follow_on.to_string();
        assert!(replay.contains("call-finish"));
        assert!(replay.contains("terminal result"));
        assert!(replay.contains("Continue after the terminal result."));
        send_final(&mut socket, "resp-follow-on").await
    });

    let workspace = temporary_workspace("terminal-tool-follow-on")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(&endpoint)
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool_with_exposure(FinishTurn, ToolExposure::DirectOnly)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .tools(tools)
        .build()?;

    let terminal = agent
        .prompt("Finish with the application tool.")
        .await?
        .result()
        .await?;
    assert!(matches!(
        terminal.completion(),
        TurnCompletion::TerminalTool { .. }
    ));

    let follow_on = agent
        .prompt("Continue after the terminal result.")
        .await?
        .result()
        .await?;
    assert!(matches!(
        follow_on.completion(),
        TurnCompletion::Message { final_message } if final_message == "done"
    ));

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    agent.shutdown().await?;
    drop((agent, events));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn oversized_terminal_receipt_fails_without_a_continuation() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let _warmup = next_json(&mut socket).await?;
        send_warmup(&mut socket, "resp-warmup").await?;

        let _generation = next_json(&mut socket).await?;
        send_json(
            &mut socket,
            completed_response(
                "resp-oversized-terminal",
                &[json!({
                    "type": "function_call",
                    "call_id": "call-oversized-finish",
                    "name": "oversized_finish_turn",
                    "arguments": "{}"
                })],
            ),
        )
        .await?;

        match timeout(
            std::time::Duration::from_millis(250),
            next_json(&mut socket),
        )
        .await
        {
            Err(_) => Ok(()),
            Ok(Ok(request)) => Err(eyre!(
                "oversized terminal receipt unexpectedly requested a continuation: {request}"
            )),
            Ok(Err(error)) => Err(error),
        }
    });

    let workspace = temporary_workspace("oversized-terminal-receipt")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(&endpoint)
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool_with_exposure(OversizedFinishTurn, ToolExposure::DirectOnly)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .tools(tools)
        .build()?;

    let result = agent
        .prompt("Finish with too much output.")
        .await?
        .result()
        .await;
    assert!(matches!(
        result,
        Err(NanocodexError::TerminalToolReceipt(
            TerminalToolReceiptError::OutputTooLarge
        ))
    ));

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    agent.shutdown().await?;
    drop((agent, events));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn failed_code_mode_cell_discards_terminal_intent_and_continues() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let _warmup = next_json(&mut socket).await?;
        send_warmup(&mut socket, "resp-warmup").await?;

        let _generation = next_json(&mut socket).await?;
        send_json(
            &mut socket,
            completed_response(
                "resp-failed-cell",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "await tools.finish_turn({}); throw new Error('fail after terminal tool');"
                })],
            ),
        )
        .await?;

        let continuation = next_json(&mut socket).await?;
        assert_eq!(continuation["input"][0]["call_id"], "call-exec");
        send_final(&mut socket, "resp-final").await
    });

    let workspace = temporary_workspace("failed-terminal-code-mode-cell")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(&endpoint)
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool(FinishTurn)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .tools(tools)
        .build()?;

    let result = agent
        .prompt("Try the application tool.")
        .await?
        .result()
        .await?;
    assert!(matches!(
        result.completion(),
        TurnCompletion::Message { final_message } if final_message == "done"
    ));

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    agent.shutdown().await?;
    drop((agent, events));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn multiple_terminal_tools_fail_the_turn_after_the_cell_commits() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let _warmup = next_json(&mut socket).await?;
        send_warmup(&mut socket, "resp-warmup").await?;

        let _generation = next_json(&mut socket).await?;
        send_json(
            &mut socket,
            completed_response(
                "resp-ambiguous-cell",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "await tools.finish_turn({}); await tools.finish_turn({});"
                })],
            ),
        )
        .await?;

        match timeout(
            std::time::Duration::from_millis(250),
            next_json(&mut socket),
        )
        .await
        {
            Err(_) => Ok(()),
            Ok(Ok(request)) => Err(eyre!(
                "ambiguous terminal cell unexpectedly requested a continuation: {request}"
            )),
            Ok(Err(error)) => Err(error),
        }
    });

    let workspace = temporary_workspace("ambiguous-terminal-code-mode-cell")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(&endpoint)
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool(FinishTurn)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .tools(tools)
        .build()?;

    let result = agent
        .prompt("Try two terminal tools.")
        .await?
        .result()
        .await;
    assert!(matches!(
        result,
        Err(NanocodexError::AmbiguousTerminalTools)
    ));

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    agent.shutdown().await?;
    drop((agent, events));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn failed_direct_tool_batch_discards_terminal_intent_and_continues() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let _warmup = next_json(&mut socket).await?;
        send_warmup(&mut socket, "resp-warmup").await?;

        let _generation = next_json(&mut socket).await?;
        send_json(
            &mut socket,
            completed_response(
                "resp-mixed-batch",
                &[
                    json!({
                        "type": "function_call",
                        "call_id": "call-finish",
                        "name": "finish_turn",
                        "arguments": "{}"
                    }),
                    json!({
                        "type": "function_call",
                        "call_id": "call-fail",
                        "name": "failing_tool",
                        "arguments": "{}"
                    }),
                ],
            ),
        )
        .await?;

        let continuation = next_json(&mut socket).await?;
        assert_eq!(continuation["input"].as_array().map(Vec::len), Some(2));
        send_final(&mut socket, "resp-final").await
    });

    let workspace = temporary_workspace("failed-direct-terminal-batch")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(&endpoint)
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool_with_exposure(FinishTurn, ToolExposure::DirectOnly)
        .tool_with_exposure(FailingTool, ToolExposure::DirectOnly)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .tools(tools)
        .build()?;

    let result = agent
        .prompt("Call both application tools.")
        .await?
        .result()
        .await?;
    assert!(matches!(
        result.completion(),
        TurnCompletion::Message { final_message } if final_message == "done"
    ));

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    agent.shutdown().await?;
    drop((agent, events));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}
