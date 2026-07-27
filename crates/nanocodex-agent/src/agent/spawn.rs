use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn build_agent<S>(
    config: Arc<ModelConfig>,
    tools: ToolsConfiguration,
    workspace: Option<PathBuf>,
    session_id: Option<SessionId>,
    prompt_cache: PromptCacheConfig,
    codex: CodexCompatibility,
    resume: Option<SessionSnapshot>,
    service_factory: ServiceFactory<S>,
) -> Result<(Nanocodex, AgentEvents)>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + Send + 'static,
    S::Error: Into<NanocodexError> + Send + 'static,
    S::Future: Send,
{
    let session_id = session_id.unwrap_or_default();
    let session_id_text = session_id.to_string();
    let PromptCacheConfig { key, shared } = prompt_cache;
    let is_resume = resume.is_some();
    let (lineage_id, prompt_cache_key, initial_resume) = if let Some(snapshot) = resume {
        let SessionResume {
            lineage_id,
            prompt_cache_key: restored_cache_key,
            workspace,
            canonical_context,
            history,
            checkpoint,
        } = snapshot.into_resume()?;
        if key
            .as_deref()
            .is_some_and(|key| key != restored_cache_key.as_ref())
        {
            return Err(NanocodexError::InvalidSessionSnapshot(
                "configured prompt cache key does not match the resumed session".to_owned(),
            ));
        }
        let initial = checkpoint.map_or_else(
            || {
                InitialResume::Rollout(Box::new(RolloutResume {
                    workspace,
                    canonical_context,
                    history,
                    prompt_cache_key: Arc::clone(&restored_cache_key),
                }))
            },
            |checkpoint| InitialResume::Exact(Box::new(checkpoint)),
        );
        (lineage_id, Some(restored_cache_key), Some(initial))
    } else {
        (
            Arc::<str>::from(session_id_text.as_str()),
            key.map(Arc::from),
            None,
        )
    };
    let configured_workspace = workspace
        .map(|path| {
            path.into_os_string()
                .into_string()
                .map(Arc::<str>::from)
                .map_err(|path| NanocodexError::WorkspaceNotUtf8 {
                    path: PathBuf::from(path),
                })
        })
        .transpose()?;
    let workspace = if let Some(initial) = initial_resume.as_ref() {
        let restored = crate::model::resolve_workspace(Some(initial.workspace()))?;
        if restored != initial.workspace() {
            return Err(NanocodexError::InvalidSessionSnapshot(
                "workspace no longer resolves to the stored location".to_owned(),
            ));
        }
        if let Some(configured) = configured_workspace {
            let requested = crate::model::resolve_workspace(Some(&configured))?;
            if requested != restored {
                return Err(NanocodexError::WorkspaceChanged {
                    current: restored,
                    requested,
                });
            }
        }
        Some(Arc::<str>::from(restored))
    } else {
        Some(Arc::<str>::from(crate::model::resolve_workspace(
            configured_workspace.as_deref(),
        )?))
    };
    let service = service_factory();
    let global_instructions = load_global_instructions(codex.home.as_deref());
    spawn_agent_driver(
        BranchSpawner {
            config,
            tools,
            lineage_id,
            prompt_cache_key,
            shared_prompt_cache: shared,
            codex_home: codex.home,
            depth: 0,
            rollout: codex.rollout,
            service_factory,
        },
        session_id,
        workspace,
        service,
        initial_resume,
        global_instructions,
        AgentOrigin {
            kind: if is_resume { "resume" } else { "root" },
            depth: 0,
            parent_session_id: None,
        },
    )
}

pub(super) fn spawn_agent_driver<S>(
    spawner: BranchSpawner<S>,
    session_id: SessionId,
    workspace: Option<Arc<str>>,
    service: S,
    initial_resume: Option<InitialResume>,
    global_instructions: Option<Arc<str>>,
    origin: AgentOrigin,
) -> Result<(Nanocodex, AgentEvents)>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + Send + 'static,
    S::Error: Into<NanocodexError> + Send + 'static,
    S::Future: Send,
{
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| NanocodexError::TokioRuntimeUnavailable)?;
    let session_id_text = session_id.to_string();
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    let tools = bind_agent_environment(
        spawner.tools.materialize(AgentHandle {
            commands: commands.downgrade(),
        })?,
        &session_id_text,
    )?;
    let rollout = spawner
        .rollout
        .as_ref()
        .map(|config| {
            let cwd = rollout_workspace(workspace.as_deref()).map_err(|source| {
                NanocodexError::InitializeRollout {
                    codex_home: config.codex_home().to_path_buf(),
                    source,
                }
            })?;
            RolloutRecorder::create(
                &runtime,
                config,
                &session_id_text,
                &cwd,
                spawner.config.system_prompt(),
                RolloutOrigin {
                    kind: origin.kind,
                    parent_thread_id: origin.parent_session_id.as_deref(),
                },
                initial_resume.as_ref().map(InitialResume::history_len),
            )
            .map_err(|source| NanocodexError::InitializeRollout {
                codex_home: config.codex_home().to_path_buf(),
                source,
            })
        })
        .transpose()?;
    let rollout_info = rollout.as_ref().map(|recorder| recorder.info().clone());
    let (events, event_stream) = EventSink::channel(session_id_text.clone());
    let initial_model = initial_resume
        .map(|initial| match initial {
            InitialResume::Exact(checkpoint) => {
                prepare_resumed_checkpoint(*checkpoint, &spawner.config, &tools, &session_id_text)
            }
            InitialResume::Rollout(resume) => {
                let RolloutResume {
                    workspace,
                    canonical_context,
                    history,
                    prompt_cache_key,
                } = *resume;
                prepare_rollout_checkpoint(
                    workspace,
                    canonical_context,
                    history,
                    prompt_cache_key,
                    &spawner.config,
                    &tools,
                    &session_id_text,
                )
            }
        })
        .transpose()?;
    let transport_stats = Arc::new(TransportStats::default());
    let agent = Nanocodex {
        commands,
        events: events.clone(),
        next_turn: Arc::new(AtomicU64::new(1)),
        lineage_id: Arc::clone(&spawner.lineage_id),
        session_id,
        rollout: rollout_info,
        rollout_recorder: rollout.clone(),
    };
    // Start discovery before returning the handle so an idle CLI or TUI immediately
    // contributes its human think time to provider prewarming.
    tools.start_providers();
    let driver = AgentDriver {
        commands: receiver,
        events,
        client: ResponsesClient::new(service),
        transport_stats,
        tools,
        workspace,
        global_instructions,
        spawner,
        initial_model,
        origin,
        rollout,
    };
    drop(runtime.spawn(async move {
        if let Err(error) = driver.run().await {
            tracing::error!(
                target: "nanocodex",
                error = %error,
                "agent driver stopped with an error"
            );
        }
    }));
    Ok((agent, event_stream))
}

pub(super) fn validate(config: &ModelConfig, prompt_cache_key: Option<&str>) -> Result<()> {
    config
        .auth
        .validate()
        .map_err(|error| NanocodexError::InvalidRequest(error.to_string()))?;
    if matches!(config.responses_transport, ResponsesTransport::WebSocket)
        && config.websocket_url.trim().is_empty()
    {
        return Err(NanocodexError::InvalidRequest(
            "Responses WebSocket URL must not be empty".to_owned(),
        ));
    }
    if matches!(config.responses_transport, ResponsesTransport::Https)
        && config.api_base_url.trim().is_empty()
    {
        return Err(NanocodexError::InvalidRequest(
            "OpenAI API base URL must not be empty".to_owned(),
        ));
    }
    if config.auth.mode() == OpenAiAuthMode::ChatGpt && config.store_responses {
        return Err(NanocodexError::InvalidRequest(
            "ChatGPT subscription authentication does not support store: true".to_owned(),
        ));
    }
    if matches!(config.responses_transport, ResponsesTransport::Https)
        && !config.store_responses
        && matches!(config.responses_history, ResponsesHistory::Incremental)
    {
        return Err(NanocodexError::InvalidRequest(
            "HTTPS with store: false requires full client-history replay".to_owned(),
        ));
    }
    if prompt_cache_key.is_some_and(|prompt_cache_key| prompt_cache_key.trim().is_empty()) {
        return Err(NanocodexError::InvalidRequest(
            "prompt_cache_key must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn rollout_workspace(workspace: Option<&str>) -> std::io::Result<PathBuf> {
    let current = std::env::current_dir()?;
    let Some(workspace) = workspace else {
        return Ok(current);
    };
    let workspace = PathBuf::from(workspace);
    if workspace.is_absolute() {
        Ok(workspace)
    } else {
        Ok(current.join(workspace))
    }
}
