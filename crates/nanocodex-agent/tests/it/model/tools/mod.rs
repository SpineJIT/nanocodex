use super::*;

mod environment;

#[tokio::test]
async fn connection_local_response_code_mode_round_trip() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let warmup = next_json(&mut socket).await?;
        assert_warmup(&warmup);
        send_json(
            &mut socket,
            json!({
                "type": "response.metadata",
                "headers": { "x-codex-turn-state": "sticky-test" }
            }),
        )
        .await?;
        send_warmup(&mut socket, "resp-warmup").await?;

        let generation = next_json(&mut socket).await?;
        assert_eq!(generation["previous_response_id"], "resp-warmup");
        assert_eq!(generation["store"], false);
        assert!(generation.get("generate").is_none());
        assert_eq!(generation["input"].as_array().map(Vec::len), Some(3));
        assert_eq!(generation["input"][0]["role"], "developer");
        assert_eq!(generation["input"][1]["role"], "user");
        assert_eq!(generation["input"][2]["role"], "user");
        assert_eq!(
            generation["client_metadata"]["x-codex-turn-state"],
            "sticky-test"
        );
        send_json(
            &mut socket,
            completed_response(
                "resp-tool",
                &[json!({
                    "id": "item-exec",
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "const result = await tools.exec_command({cmd: \"printf hello\"}); text(result.output);"
                })],
            ),
        )
        .await?;

        let continuation = next_json(&mut socket).await?;
        assert_eq!(continuation["previous_response_id"], "resp-tool");
        assert_eq!(continuation["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(continuation["input"][0]["type"], "custom_tool_call_output");
        assert_eq!(continuation["input"][0]["call_id"], "call-exec");
        assert!(continuation["input"][0].get("success").is_none());
        assert!(
            continuation["input"][0]["output"]
                .as_array()
                .is_some_and(|content| content.iter().any(|item| {
                    item["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("hello"))
                }))
        );
        send_final(&mut socket, "resp-final").await
    });

    let workspace = temporary_workspace("code-mode")?;
    let output = run_model(&endpoint, &workspace, "run a shell command").await?;
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    assert!(output.contains("\"tool\":\"exec\""));
    assert!(output.contains("\"tool\":\"exec_command\""));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn unsupported_direct_tools_return_failed_results_to_the_model() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        assert_warmup(&next_json(&mut socket).await?);
        send_warmup(&mut socket, "resp-warmup").await?;

        let generation = next_json(&mut socket).await?;
        assert_eq!(generation["previous_response_id"], "resp-warmup");
        send_json(
            &mut socket,
            completed_response(
                "resp-unsupported",
                &[
                    json!({
                        "type": "custom_tool_call",
                        "call_id": "call-custom",
                        "name": "missing_custom",
                        "input": "raw input"
                    }),
                    json!({
                        "type": "function_call",
                        "call_id": "call-function",
                        "namespace": "example::",
                        "name": "missing_function",
                        "arguments": "not json"
                    }),
                ],
            ),
        )
        .await?;

        let continuation = next_json(&mut socket).await?;
        assert_eq!(continuation["previous_response_id"], "resp-unsupported");
        let input = continuation["input"]
            .as_array()
            .ok_or_else(|| eyre!("continuation input was not an array"))?;
        assert_eq!(input[0]["type"], "custom_tool_call_output");
        assert_eq!(input[0]["call_id"], "call-custom");
        assert_eq!(
            input[0]["output"],
            "unsupported custom tool call: missing_custom"
        );
        assert!(
            input[0]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("ctco_"))
        );
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call-function");
        assert_eq!(
            input[1]["output"],
            "unsupported call: example::missing_function"
        );
        assert!(
            input[1]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("fco_"))
        );
        send_final(&mut socket, "resp-final").await
    });

    let workspace = temporary_workspace("unsupported-tools")?;
    let output = run_model(&endpoint, &workspace, "recover from unsupported tools").await?;
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    assert_eq!(
        output.matches(r#""status":"failed""#).count(),
        2,
        "{output}"
    );
    assert!(output.contains("\"tool_calls\":2"));
    assert!(output.contains("\"run.completed\""));
    assert!(!output.contains("\"run.failed\""));
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn code_mode_notify_adds_a_named_exec_output_to_the_next_request() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        assert_warmup(&next_json(&mut socket).await?);
        send_warmup(&mut socket, "resp-warmup").await?;

        let generation = next_json(&mut socket).await?;
        assert_eq!(generation["previous_response_id"], "resp-warmup");
        send_json(
            &mut socket,
            completed_response(
                "resp-notify",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "notify({phase: \"working\"}); text(\"done\");"
                })],
            ),
        )
        .await?;

        let continuation = next_json(&mut socket).await?;
        assert_eq!(continuation["previous_response_id"], "resp-notify");
        let input = continuation["input"]
            .as_array()
            .ok_or_else(|| eyre!("continuation input was not an array"))?;
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "custom_tool_call_output");
        assert_eq!(input[0]["call_id"], "call-exec");
        assert!(input[0].get("name").is_none());
        assert!(input[0].to_string().contains("done"));
        assert_eq!(input[1]["type"], "custom_tool_call_output");
        assert_eq!(input[1]["call_id"], "call-exec");
        assert_eq!(input[1]["name"], "exec");
        assert_eq!(input[1]["output"], r#"{"phase":"working"}"#);
        assert!(input[1].get("success").is_none());
        send_final(&mut socket, "resp-final").await
    });

    let workspace = temporary_workspace("code-mode-notify")?;
    run_model(&endpoint, &workspace, "send a progress notification").await?;
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn prepares_images_and_stops_on_invalid_image_requests() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        assert_warmup(&next_json(&mut socket).await?);
        send_warmup(&mut socket, "resp-warmup").await?;

        let generation = next_json(&mut socket).await?;
        assert_eq!(generation["previous_response_id"], "resp-warmup");
        send_json(
            &mut socket,
            completed_response(
                "resp-image",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-image",
                    "name": "exec",
                    "input": "image(\"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=\", \"original\");"
                })],
            ),
        )
        .await?;

        let continuation = next_json(&mut socket).await?;
        let output = continuation["input"][0]["output"]
            .as_array()
            .ok_or_else(|| eyre!("image tool output was not content"))?;
        let image = output
            .iter()
            .find(|item| item["type"] == "input_image")
            .ok_or_else(|| eyre!("prepared image was missing"))?;
        assert!(
            image["image_url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,"))
        );
        assert!(image.get("detail").is_none());

        send_json(
            &mut socket,
            json!({
                "type": "response.failed",
                "response": {
                    "id": "resp-invalid-image",
                    "status": "failed",
                    "error": {
                        "code": "invalid_image",
                        "message": "The image data you provided does not represent a valid image"
                    }
                }
            }),
        )
        .await?;

        Ok::<(), eyre::Report>(())
    });

    let workspace = temporary_workspace("images")?;
    let error = run_model(&endpoint, &workspace, "inspect images")
        .await
        .expect_err("invalid tool image should fail the turn");
    let error = error
        .downcast_ref::<NanocodexError>()
        .ok_or_else(|| eyre!("invalid image returned the wrong error type"))?;
    assert!(matches!(
        error.responses_error(),
        Some(ResponsesError::InvalidImageRequest { .. })
    ));
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn yielded_exec_cell_continues_through_direct_wait_tool() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        assert_warmup(&next_json(&mut socket).await?);
        send_warmup(&mut socket, "resp-warmup").await?;

        let generation = next_json(&mut socket).await?;
        assert_eq!(generation["previous_response_id"], "resp-warmup");
        send_json(
            &mut socket,
            completed_response(
                "resp-exec",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "text(\"before\"); await yield_control(); const result = await tools.exec_command({cmd: \"printf after\", login: false}); text(result.output);"
                })],
            ),
        )
        .await?;

        let yielded = next_json(&mut socket).await?;
        assert_eq!(yielded["previous_response_id"], "resp-exec");
        assert_eq!(yielded["input"][0]["type"], "custom_tool_call_output");
        assert!(
            yielded
                .to_string()
                .contains("Script running with cell ID 1")
        );
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

        let completed = next_json(&mut socket).await?;
        assert_eq!(completed["previous_response_id"], "resp-wait");
        assert_eq!(completed["input"][0]["type"], "function_call_output");
        assert_eq!(completed["input"][0]["call_id"], "call-wait");
        assert!(completed.to_string().contains("after"));
        send_final(&mut socket, "resp-final").await
    });

    let workspace = temporary_workspace("code-mode-wait")?;
    let output = run_model(&endpoint, &workspace, "yield and wait").await?;
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    assert!(output.contains("\"tool\":\"wait\""));
    let nested_call = output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| {
            event["type"] == "tool.call" && event["payload"]["call_id"] == "call-exec/code-1"
        })
        .ok_or_else(|| eyre!("nested call did not retain its original exec lineage"))?;
    assert_eq!(nested_call["payload"]["model_call_index"], 1);
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}
