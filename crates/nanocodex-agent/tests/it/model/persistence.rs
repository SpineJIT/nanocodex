use super::*;

#[tokio::test]
async fn missing_stored_checkpoint_replays_local_history_once() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut root = accept_async(stream).await?;
        assert_warmup(&next_json(&mut root).await?);
        send_warmup(&mut root, "resp-warmup").await?;
        let first = next_json(&mut root).await?;
        send_final(&mut root, "resp-first").await?;

        let (stream, _) = listener.accept().await?;
        let mut branch = accept_async(stream).await?;
        let checkpoint = next_json(&mut branch).await?;
        assert_eq!(checkpoint["previous_response_id"], "resp-first");
        assert_eq!(checkpoint["input"].as_array().map(Vec::len), Some(1));
        send_json(
            &mut branch,
            json!({
                "type": "error",
                "error": {
                    "code": "previous_response_not_found",
                    "message": "checkpoint expired"
                }
            }),
        )
        .await?;

        let replay = next_json(&mut branch).await?;
        assert!(replay.get("previous_response_id").is_none());
        assert_eq!(replay["store"], true);
        assert_eq!(replay["input"][0]["type"], "additional_tools");
        assert_eq!(replay["input"][1]["role"], "developer");
        let replay_text = replay.to_string();
        assert!(replay_text.contains("root prompt"));
        assert!(replay_text.contains("branch after eviction"));
        assert!(
            replay["input"]
                .as_array()
                .is_some_and(|items| items.len() > 4)
        );
        send_final(&mut branch, "resp-replayed").await?;
        drop((root, first));
        Result::<()>::Ok(())
    });

    let workspace = temporary_workspace("checkpoint-miss")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(endpoint)
        .build()?;
    let (agent, root_events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .build()?;
    let first = agent
        .prompt(Prompt::new("root prompt"))
        .await?
        .result()
        .await?;
    let (fork, mut fork_events) = agent.fork_from(&first).await?;
    let branch = fork.prompt("branch after eviction").await?;
    assert_eq!(branch.result().await?.final_message(), "done");

    drop((agent, fork, root_events));
    let mut observed_checkpoint_retry = false;
    while let Some(event) = fork_events.recv().await {
        if event.kind == AgentEventKind::ModelAttemptRetrying {
            let payload = event.decode_payload::<Value>()?;
            observed_checkpoint_retry = payload["error_class"] == "checkpoint_missing"
                && payload["replay_mode"] == "full_history"
                && payload["opens_new_socket"] == false;
        }
    }
    assert!(observed_checkpoint_retry);
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn serialized_session_and_codex_rollout_share_committed_history() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut original = accept_async(stream).await?;
        let warmup = next_json(&mut original).await?;
        assert_eq!(warmup["prompt_cache_key"], "durable-cache");
        send_warmup(&mut original, "resp-warmup").await?;
        let first = next_json(&mut original).await?;
        assert_eq!(first["previous_response_id"], "resp-warmup");
        send_final(&mut original, "resp-first").await?;

        let (stream, _) = listener.accept().await?;
        let mut resumed = accept_async(stream).await?;
        let replay = next_json(&mut resumed).await?;
        assert!(replay.get("previous_response_id").is_none());
        assert_eq!(
            replay["prompt_cache_key"],
            "019c0d31-c308-7d91-bff4-5dca82d15ac6"
        );
        assert_eq!(replay["input"][0]["type"], "additional_tools");
        assert!(replay["input"][0].get("id").is_none());
        assert_eq!(replay["input"][1]["role"], "developer");
        assert_eq!(
            replay["input"][1]["content"][0]["text"],
            "durable instructions"
        );
        let replay_text = replay.to_string();
        assert!(replay_text.contains("first prompt"));
        assert!(replay_text.contains("resume prompt"));
        send_final(&mut resumed, "resp-resumed").await?;
        Result::<()>::Ok(())
    });

    let workspace = temporary_workspace("serialized-resume")?;
    let rollout_home = temporary_workspace("serialized-resume-rollout")?;
    let openai = || {
        OpenAi::builder("test-key")
            .websocket_url(endpoint.clone())
            .build()
    };
    let (agent, events) = Nanocodex::builder(openai()?)
        .instructions("durable instructions")
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .prompt_cache_key("durable-cache")
        .rollout(RolloutConfig::new(&rollout_home))
        .build()?;
    let rollout_path = agent
        .rollout()
        .ok_or_else(|| eyre!("rollout was not configured"))?
        .path()
        .to_path_buf();
    let first = agent.prompt("first prompt").await?.result().await?;
    let encoded = serde_json::to_vec(&first.snapshot())?;
    agent.flush_rollout().await?;
    let durable_config = RolloutConfig::new(&rollout_home);
    let durable = durable_config.load_session("019c0d31-c308-7d91-bff4-5dca82d15ac6")?;
    assert_eq!(durable.thread_id(), agent.session_id().to_string());
    assert_eq!(
        Path::new(durable.workspace()).canonicalize()?,
        workspace.canonicalize()?
    );
    assert_eq!(durable.rollout_path(), rollout_path.canonicalize()?);
    let snapshot_json = serde_json::from_slice::<Value>(&encoded)?;
    let request_prefix = snapshot_json["request_prefix"]
        .as_array()
        .ok_or_else(|| eyre!("snapshot request prefix was not an array"))?;
    assert_eq!(request_prefix[0]["type"], "additional_tools");
    assert!(request_prefix[0].get("id").is_none());
    assert!(request_prefix[1]["id"].is_string());
    assert!(
        snapshot_json["history"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| {
                item.get("id").is_some_and(Value::is_string) || item["type"] == "compaction_trigger"
            }))
    );
    let rollout_history = std::fs::read_to_string(&rollout_path)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<serde_json::Result<Vec<_>>>()?
        .into_iter()
        .filter(|line| line["type"] == "response_item")
        .map(|line| line["payload"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(durable.snapshot())?["history"].as_array(),
        Some(&rollout_history),
        "rollout resume must materialize the recorded committed history"
    );
    let snapshot: SessionSnapshot = serde_json::from_slice(&encoded)?;
    drop((agent, events, first));

    let mut unsupported: Value = serde_json::from_slice(&encoded)?;
    unsupported["version"] = json!(2);
    let unsupported: SessionSnapshot = serde_json::from_value(unsupported)?;
    let unsupported = Nanocodex::builder(openai()?).resume(unsupported).build();
    assert!(matches!(
        unsupported,
        Err(NanocodexError::InvalidSessionSnapshot(message))
            if message.contains("unsupported format version")
    ));

    let incompatible = Nanocodex::builder(openai()?)
        .instructions("changed instructions")
        .thinking(Thinking::Low)
        .resume(snapshot.clone())
        .build();
    assert!(matches!(
        incompatible,
        Err(NanocodexError::InvalidSessionSnapshot(message))
            if message.contains("instructions or tool definitions")
    ));
    let other_workspace = temporary_workspace("serialized-resume-other")?;
    let incompatible = Nanocodex::builder(openai()?)
        .instructions("durable instructions")
        .thinking(Thinking::Low)
        .workspace(&other_workspace)
        .resume(snapshot.clone())
        .build();
    assert!(matches!(
        incompatible,
        Err(NanocodexError::WorkspaceChanged { .. })
    ));
    std::fs::remove_dir_all(other_workspace)?;
    let incompatible = Nanocodex::builder(openai()?)
        .instructions("durable instructions")
        .thinking(Thinking::Low)
        .prompt_cache_key("changed-cache")
        .resume(snapshot.clone())
        .build();
    assert!(matches!(
        incompatible,
        Err(NanocodexError::InvalidSessionSnapshot(message))
            if message.contains("prompt cache key")
    ));

    let (thread_id, snapshot, rollout) = durable.into_parts();
    let (resumed, resumed_events) = Nanocodex::builder(openai()?)
        .instructions("durable instructions")
        .thinking(Thinking::Low)
        .session_id(thread_id.parse()?)
        .resume(snapshot)
        .rollout(rollout)
        .build()?;
    assert_eq!(
        resumed_events.request_id(),
        "019c0d31-c308-7d91-bff4-5dca82d15ac6"
    );
    assert_eq!(
        resumed
            .prompt("resume prompt")
            .await?
            .result()
            .await?
            .final_message(),
        "done"
    );
    resumed.flush_rollout().await?;
    assert_eq!(
        resumed
            .rollout()
            .map(|rollout| rollout.path().canonicalize())
            .transpose()?,
        Some(rollout_path.canonicalize()?)
    );
    let durable = durable_config.load_session("019c0d31-c308-7d91-bff4-5dca82d15ac6")?;
    let durable_json = serde_json::to_value(durable.snapshot())?;
    assert!(
        durable_json["history"]
            .to_string()
            .contains("resume prompt")
    );
    let session_meta_count = std::fs::read_to_string(&rollout_path)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<serde_json::Result<Vec<_>>>()?
        .into_iter()
        .filter(|line| line["type"] == "session_meta")
        .count();
    assert_eq!(session_meta_count, 1);

    drop((resumed, resumed_events));
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    std::fs::remove_dir_all(rollout_home)?;
    Ok(())
}

#[tokio::test]
async fn serialized_session_resumes_over_ephemeral_https() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let first = next_http_json(&listener).await?;
        assert_eq!(first.body["store"], false);
        assert!(first.body.get("previous_response_id").is_none());
        assert!(first.body.to_string().contains("first prompt"));
        send_http_final(first.stream, "resp-first").await?;

        let resumed = next_http_json(&listener).await?;
        assert_eq!(resumed.body["store"], false);
        assert!(resumed.body.get("previous_response_id").is_none());
        let replay = resumed.body.to_string();
        assert!(replay.contains("first prompt"));
        assert!(replay.contains("done"));
        assert!(replay.contains("resume prompt"));
        send_http_final(resumed.stream, "resp-resumed").await
    });

    let workspace = temporary_workspace("serialized-resume-https")?;
    let openai = OpenAi::builder("test-key")
        .transport(ResponsesTransport::Https)
        .store(false)
        .api_base_url(endpoint.clone())
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .instructions("durable instructions")
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .prompt_cache_key("durable-cache")
        .build()?;
    let first = agent.prompt("first prompt").await?.result().await?;
    let snapshot = serde_json::from_slice(&serde_json::to_vec(&first.snapshot())?)?;
    drop((agent, events, first));

    let openai = OpenAi::builder("test-key")
        .transport(ResponsesTransport::Https)
        .store(false)
        .api_base_url(endpoint)
        .build()?;
    let (resumed, resumed_events) = Nanocodex::builder(openai)
        .instructions("durable instructions")
        .thinking(Thinking::Low)
        .resume(snapshot)
        .build()?;
    assert_eq!(
        resumed
            .prompt("resume prompt")
            .await?
            .result()
            .await?
            .final_message(),
        "done"
    );

    drop((resumed, resumed_events));
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock HTTPS Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}
