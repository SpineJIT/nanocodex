use super::*;

#[tokio::test]
async fn cancellation_retains_interrupted_prompt_and_resumes_from_the_abort_boundary() -> Result<()>
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let (second_seen, second_seen_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut first_socket = accept_async(stream).await?;
        assert_warmup(&next_json(&mut first_socket).await?);
        send_warmup(&mut first_socket, "resp-warmup").await?;

        let first = next_json(&mut first_socket).await?;
        assert_eq!(first["previous_response_id"], "resp-warmup");
        send_final(&mut first_socket, "resp-first").await?;

        let cancelled = next_json(&mut first_socket).await?;
        assert_eq!(cancelled["previous_response_id"], "resp-first");
        assert_eq!(cancelled["input"][0]["content"][0]["text"], "cancel me");
        second_seen
            .send(())
            .map_err(|()| eyre!("second-request signal receiver dropped"))?;
        send_json(
            &mut first_socket,
            json!({
                "type": "response.output_text.delta",
                "delta": "partial text that must not enter history"
            }),
        )
        .await?;

        let (stream, _) = listener.accept().await?;
        let mut replacement = accept_async(stream).await?;
        let queued = next_json(&mut replacement).await?;
        assert_interrupted_replay(&queued);
        send_final(&mut replacement, "resp-follow-up").await
    });

    let workspace = temporary_workspace("cancel-turn")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(endpoint)
        .build()?;
    let (agent, mut events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .build()?;

    let first = agent.prompt(Prompt::new("first prompt")).await?;
    assert_eq!(first.result().await?.final_message(), "done");

    let cancelled = agent.prompt("cancel me").await?;
    second_seen_rx
        .await
        .map_err(|_| eyre!("second request was not observed"))?;
    let queued = agent.prompt("cancel before running").await?;
    let queued_control = queued.control();
    let follow_up = agent.prompt("run after cancellations").await?;

    assert!(matches!(
        queued.steer("wrong target").await,
        Err(NanocodexError::TurnNotSteerable)
    ));
    queued.cancel().await?;
    assert!(matches!(
        queued_control.cancel().await,
        Err(NanocodexError::TurnNotCancellable)
    ));

    let cancellation = cancelled.control();
    cancellation.cancel().await?;
    assert!(matches!(
        cancelled.result().await,
        Err(NanocodexError::TurnCancelled)
    ));
    assert!(matches!(
        queued.result().await,
        Err(NanocodexError::TurnCancelled)
    ));
    assert!(matches!(
        cancellation.cancel().await,
        Err(NanocodexError::TurnNotCancellable)
    ));
    assert_eq!(follow_up.result().await?.final_message(), "done");
    drop((queued_control, cancellation, agent));

    let mut terminal_statuses = Vec::new();
    while let Some(event) = events.recv().await {
        match event.kind {
            AgentEventKind::RunCompleted | AgentEventKind::RunFailed => {
                let payload = event.decode_payload::<Value>()?;
                terminal_statuses.push(payload["status"].as_str().unwrap_or_default().to_owned());
            }
            _ => {}
        }
    }
    assert_eq!(
        terminal_statuses,
        ["completed", "cancelled", "cancelled", "completed"]
    );

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

fn assert_interrupted_replay(request: &Value) {
    assert!(request.get("previous_response_id").is_none());
    assert_eq!(request["input"].as_array().map(Vec::len), Some(9));
    assert_eq!(request["input"][0]["type"], "additional_tools");
    assert_eq!(request["input"][1]["role"], "developer");
    assert_eq!(request["input"][2]["role"], "developer");
    assert_eq!(request["input"][3]["role"], "user");
    assert_eq!(request["input"][4]["content"][0]["text"], "first prompt");
    assert_eq!(request["input"][5]["content"][0]["text"], "done");
    assert_eq!(request["input"][6]["content"][0]["text"], "cancel me");
    assert!(
        request["input"][7]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("<turn_aborted>"))
    );
    assert_eq!(
        request["input"][8]["content"][0]["text"],
        "run after cancellations"
    );
    assert!(
        !request
            .to_string()
            .contains("partial text that must not enter history")
    );
}

#[tokio::test]
async fn cancellation_during_pre_turn_compaction_retains_the_accepted_prompt() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let (compact_seen, compact_seen_rx) = tokio::sync::oneshot::channel();
    let (cancelled, cancelled_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut first_socket = accept_async(stream).await?;
        assert_warmup(&next_json(&mut first_socket).await?);
        send_warmup(&mut first_socket, "resp-warmup").await?;

        drop(next_json(&mut first_socket).await?);
        send_json(
            &mut first_socket,
            completed_response_with_usage(
                "resp-first",
                &[json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "first done" }]
                })],
                244_800,
            ),
        )
        .await?;

        let interrupted_compaction = next_json(&mut first_socket).await?;
        assert_eq!(interrupted_compaction["previous_response_id"], "resp-first");
        assert!(
            !interrupted_compaction
                .to_string()
                .contains("cancel during compaction")
        );
        compact_seen
            .send(())
            .map_err(|()| eyre!("compaction signal receiver dropped"))?;
        cancelled_rx
            .await
            .map_err(|_| eyre!("cancellation signal sender dropped"))?;
        drop(first_socket);

        let (stream, _) = listener.accept().await?;
        let mut replacement = accept_async(stream).await?;
        let retried_compaction = next_json(&mut replacement).await?;
        assert!(retried_compaction.get("previous_response_id").is_none());
        let encoded = retried_compaction.to_string();
        let prompt_index = encoded
            .find("cancel during compaction")
            .ok_or_else(|| eyre!("cancelled prompt missing from replayed history"))?;
        let abort_index = encoded
            .find("<turn_aborted>")
            .ok_or_else(|| eyre!("abort marker missing from replayed history"))?;
        assert!(prompt_index < abort_index);
        assert!(!encoded.contains("continue after cancellation"));
        send_json(
            &mut replacement,
            json!({
                "type": "response.output_item.done",
                "item": {
                    "id": "cmp-server-id",
                    "type": "compaction",
                    "encrypted_content": "opaque-summary"
                }
            }),
        )
        .await?;
        send_json(
            &mut replacement,
            completed_response_with_usage("resp-compact", &[], 120),
        )
        .await?;

        let continuation = next_json(&mut replacement).await?;
        assert!(continuation.get("previous_response_id").is_none());
        assert!(
            continuation
                .to_string()
                .contains("continue after cancellation")
        );
        send_final(&mut replacement, "resp-final").await
    });

    let workspace = temporary_workspace("cancel-pre-turn-compaction")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(endpoint)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .build()?;
    assert_eq!(
        agent
            .prompt("first prompt")
            .await?
            .result()
            .await?
            .final_message(),
        "first done"
    );

    let interrupted = agent.prompt("cancel during compaction").await?;
    compact_seen_rx
        .await
        .map_err(|_| eyre!("pre-turn compaction was not observed"))?;
    interrupted.cancel().await?;
    assert!(matches!(
        interrupted.result().await,
        Err(NanocodexError::TurnCancelled)
    ));
    cancelled
        .send(())
        .map_err(|()| eyre!("cancellation signal receiver dropped"))?;

    assert_eq!(
        agent
            .prompt("continue after cancellation")
            .await?
            .result()
            .await?
            .final_message(),
        "done"
    );
    drop(agent);
    drop(events);
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn cancellation_pairs_an_active_tool_call_before_resuming() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut first = accept_async(stream).await?;
        assert_warmup(&next_json(&mut first).await?);
        send_warmup(&mut first, "resp-warmup").await?;

        let generation = next_json(&mut first).await?;
        assert_eq!(generation["previous_response_id"], "resp-warmup");
        send_json(
            &mut first,
            completed_response(
                "resp-tool",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "const result = await tools.exec_command({cmd: \"printf started > tool-started; sleep 30\"}); text(result.output);"
                })],
            ),
        )
        .await?;

        let (stream, _) = listener.accept().await?;
        let mut replacement = accept_async(stream).await?;
        let resumed = next_json(&mut replacement).await?;
        assert!(resumed.get("previous_response_id").is_none());
        assert_eq!(resumed["input"].as_array().map(Vec::len), Some(9));
        assert_eq!(resumed["input"][4]["content"][0]["text"], "run a long tool");
        assert_eq!(resumed["input"][5]["type"], "custom_tool_call");
        assert_eq!(resumed["input"][5]["call_id"], "call-exec");
        assert_eq!(resumed["input"][6]["type"], "custom_tool_call_output");
        assert_eq!(resumed["input"][6]["call_id"], "call-exec");
        assert!(resumed["input"][6].to_string().contains("aborted by user"));
        assert!(
            resumed["input"][7]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("<turn_aborted>"))
        );
        assert_eq!(resumed["input"][8]["content"][0]["text"], "continue");
        send_final(&mut replacement, "resp-follow-up").await
    });

    let workspace = temporary_workspace("cancel-tool")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(endpoint)
        .build()?;
    let (agent, mut events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .build()?;

    let interrupted = agent.prompt("run a long tool").await?;
    loop {
        let event = events
            .recv()
            .await
            .ok_or_else(|| eyre!("event stream closed before the tool call"))?;
        if event.kind == AgentEventKind::ToolCall {
            break;
        }
    }
    timeout(std::time::Duration::from_secs(5), async {
        while !workspace.join("tool-started").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| eyre!("tool process did not start"))?;

    interrupted.cancel().await?;
    assert!(matches!(
        interrupted.result().await,
        Err(NanocodexError::TurnCancelled)
    ));
    assert_eq!(
        agent
            .prompt("continue")
            .await?
            .result()
            .await?
            .final_message(),
        "done"
    );
    drop(agent);

    let mut saw_cancelled_tool = false;
    while let Some(event) = events.recv().await {
        if event.kind == AgentEventKind::ToolResult {
            let payload = event.decode_payload::<Value>()?;
            saw_cancelled_tool |= payload["call_id"] == "call-exec"
                && payload["status"] == "cancelled"
                && payload.to_string().contains("aborted by user");
        }
    }
    assert!(saw_cancelled_tool);
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}
