use super::*;

#[tokio::test]
async fn websocket_ephemeral_chains_on_connection_and_replays_a_fresh_fork() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut root = accept_async(stream).await?;
        let warmup = next_json(&mut root).await?;
        assert_eq!(warmup["store"], false);
        assert_eq!(warmup["generate"], false);
        send_warmup(&mut root, "resp-warmup").await?;

        let first = next_json(&mut root).await?;
        assert_eq!(first["store"], false);
        assert_eq!(first["previous_response_id"], "resp-warmup");
        send_final(&mut root, "resp-first").await?;

        let second = next_json(&mut root).await?;
        assert_eq!(second["previous_response_id"], "resp-first");
        assert_eq!(second["input"].as_array().map(Vec::len), Some(1));
        send_final(&mut root, "resp-second").await?;

        let (stream, _) = listener.accept().await?;
        let mut branch = accept_async(stream).await?;
        let replay = next_json(&mut branch).await?;
        assert_eq!(replay["store"], false);
        assert!(replay.get("previous_response_id").is_none());
        let replay = replay.to_string();
        assert!(replay.contains("first prompt"));
        assert!(replay.contains("branch prompt"));
        send_final(&mut branch, "resp-branch").await
    });

    let workspace = temporary_workspace("websocket-ephemeral-fork")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(endpoint)
        .store(false)
        .build()?;
    let (agent, root_events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .build()?;
    let first = agent.prompt("first prompt").await?.result().await?;
    assert_eq!(
        agent
            .prompt("second prompt")
            .await?
            .result()
            .await?
            .final_message(),
        "done"
    );
    let (fork, fork_events) = agent.fork_from(&first).await?;
    assert_eq!(
        fork.prompt("branch prompt")
            .await?
            .result()
            .await?
            .final_message(),
        "done"
    );
    drop((agent, fork, root_events, fork_events));
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn websocket_full_replay_never_sends_a_previous_response_id() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let warmup = next_json(&mut socket).await?;
        send_warmup(&mut socket, "resp-warmup").await?;

        let first = next_json(&mut socket).await?;
        assert!(first.get("previous_response_id").is_none());
        assert!(first.to_string().contains("first prompt"));
        send_final(&mut socket, "resp-first").await?;

        let second = next_json(&mut socket).await?;
        assert!(second.get("previous_response_id").is_none());
        let replay = second.to_string();
        assert!(replay.contains("first prompt"));
        assert!(replay.contains("second prompt"));
        send_final(&mut socket, "resp-second").await?;
        drop(warmup);
        Result::<()>::Ok(())
    });

    let workspace = temporary_workspace("websocket-full-replay")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(endpoint)
        .store(false)
        .history(ResponsesHistory::FullReplay)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .build()?;
    agent.prompt("first prompt").await?.result().await?;
    agent.prompt("second prompt").await?.result().await?;
    drop((agent, events));
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn follow_on_prompts_can_change_turn_policy_without_restarting_the_session() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let warmup = next_json(&mut socket).await?;
        assert_warmup(&warmup);
        assert_eq!(warmup["reasoning"]["effort"], "low");
        assert_eq!(warmup["input"][1]["content"][0]["text"], "custom prompt");
        send_warmup(&mut socket, "resp-warmup").await?;

        let first = next_json(&mut socket).await?;
        assert_eq!(first["previous_response_id"], "resp-warmup");
        assert_eq!(first["reasoning"]["effort"], "low");
        assert!(first.get("service_tier").is_none());
        let prompt_cache_key = first["prompt_cache_key"].clone();
        send_final(&mut socket, "resp-first").await?;

        let follow_on = next_json(&mut socket).await?;
        assert_eq!(follow_on["previous_response_id"], "resp-first");
        assert_eq!(follow_on["reasoning"]["effort"], "high");
        assert_eq!(follow_on["service_tier"], "priority");
        assert_eq!(follow_on["prompt_cache_key"], prompt_cache_key);
        assert_eq!(follow_on["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(follow_on["input"][0]["role"], "user");
        assert_eq!(follow_on["input"][0]["content"][0]["text"], "second prompt");
        send_final(&mut socket, "resp-second").await?;

        let standard = next_json(&mut socket).await?;
        assert_eq!(standard["previous_response_id"], "resp-second");
        assert_eq!(standard["reasoning"]["effort"], "high");
        assert!(standard.get("service_tier").is_none());
        send_final(&mut socket, "resp-third").await
    });

    let workspace = temporary_workspace("follow-on")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(endpoint)
        .build()?;
    let (agent, mut events) = Nanocodex::builder(openai)
        .instructions("custom prompt")
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .build()?;

    let first = agent.prompt(Prompt::new("first prompt")).await?;
    assert_eq!(first.result().await?.final_message(), "done");
    agent.set_thinking(Thinking::High).await?;
    agent.set_fast_mode(true).await?;
    let second = agent.prompt(Prompt::new("second prompt")).await?;
    assert_eq!(second.result().await?.final_message(), "done");
    agent.set_fast_mode(false).await?;
    let third = agent.prompt(Prompt::new("third prompt")).await?;
    assert_eq!(third.result().await?.final_message(), "done");
    drop(agent);

    let mut completed = Vec::new();
    while let Some(event) = events.recv().await {
        if event.kind == AgentEventKind::RunCompleted {
            completed.push(event.decode_payload::<Value>()?);
        }
    }
    assert_eq!(completed.len(), 3);
    assert_eq!(completed[0]["connection_attempts"], 1);
    assert_eq!(completed[0]["response_attempts"], 2);
    assert_eq!(completed[0]["effort"], "low");
    assert_eq!(completed[1]["connection_attempts"], 0);
    assert_eq!(completed[1]["response_attempts"], 1);
    assert_eq!(completed[1]["effort"], "high");
    assert_eq!(completed[2]["connection_attempts"], 0);
    assert_eq!(completed[2]["response_attempts"], 1);
    assert_eq!(completed[2]["effort"], "high");

    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
async fn queued_prompts_retain_the_thinking_captured_when_accepted() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let (first_started, first_started_rx) = tokio::sync::oneshot::channel();
    let (release_first, release_first_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        let warmup = next_json(&mut socket).await?;
        assert_eq!(warmup["reasoning"]["effort"], "low");
        send_warmup(&mut socket, "resp-warmup").await?;

        let first = next_json(&mut socket).await?;
        assert_eq!(first["reasoning"]["effort"], "low");
        first_started
            .send(())
            .map_err(|()| eyre!("first request signal receiver dropped"))?;
        release_first_rx
            .await
            .map_err(|_| eyre!("first request release sender dropped"))?;
        send_json(
            &mut socket,
            completed_response(
                "resp-first-tool",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-exec",
                    "name": "exec",
                    "input": "text(\"continued\")"
                })],
            ),
        )
        .await?;

        let continuation = next_json(&mut socket).await?;
        assert_eq!(continuation["previous_response_id"], "resp-first-tool");
        assert_eq!(continuation["reasoning"]["effort"], "low");
        send_final(&mut socket, "resp-first").await?;

        let queued = next_json(&mut socket).await?;
        assert_eq!(queued["previous_response_id"], "resp-first");
        assert_eq!(queued["reasoning"]["effort"], "low");
        assert!(queued.get("service_tier").is_none());
        send_final(&mut socket, "resp-queued").await?;

        let updated = next_json(&mut socket).await?;
        assert_eq!(updated["previous_response_id"], "resp-queued");
        assert_eq!(updated["reasoning"]["effort"], "high");
        assert_eq!(updated["service_tier"], "priority");
        send_final(&mut socket, "resp-updated").await
    });

    let workspace = temporary_workspace("queued-thinking")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(endpoint)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .build()?;

    let first = agent.prompt("first prompt").await?;
    first_started_rx
        .await
        .map_err(|_| eyre!("first request was not observed"))?;
    let queued = agent.prompt("queued prompt").await?;
    agent.set_thinking(Thinking::High).await?;
    agent.set_fast_mode(true).await?;
    release_first
        .send(())
        .map_err(|()| eyre!("first request release receiver dropped"))?;
    first.result().await?;
    queued.result().await?;
    agent.prompt("updated prompt").await?.result().await?;

    drop((agent, events));
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}
