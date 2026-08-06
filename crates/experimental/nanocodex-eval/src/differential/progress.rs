use super::*;

#[derive(Clone, Default)]
pub(super) struct DiffProgress {
    active: Option<ActiveDiffProgress>,
}

#[derive(Clone)]
pub(super) struct ActiveDiffProgress {
    sender: mpsc::UnboundedSender<PendingProgressRecord>,
    started: Instant,
    api_diff: Arc<Mutex<LiveApiDiff>>,
}

pub(super) struct DiffProgressRecorder {
    path: PathBuf,
    task: JoinHandle<std::io::Result<()>>,
}

pub(super) struct PendingProgressRecord {
    observed_at: DateTime<Utc>,
    elapsed_ms: u64,
    arm: &'static str,
    kind: String,
    summary: Option<String>,
    coordinate: Option<ProgressCoordinate>,
}

#[derive(Clone, Serialize)]
pub(super) struct ProgressCoordinate {
    task_name: String,
    task_content_digest: String,
    thinking: String,
    nanocodex_tool_mode: String,
    codex_tool_mode: String,
    trial: usize,
}

pub(super) struct LaneProgressState {
    pub(super) elapsed_ms: u64,
    pub(super) kind: String,
    pub(super) summary: Option<String>,
}

#[derive(Default)]
pub(super) struct LiveApiDiff {
    arms: BTreeMap<&'static str, LiveApiArm>,
    compared_requests: usize,
    compared_responses: usize,
}

#[derive(Default)]
pub(super) struct LiveApiArm {
    pub(super) turns: Vec<LiveApiTurn>,
    source_offsets: BTreeMap<u64, usize>,
    active_offset: Option<usize>,
    initial_request_seen: bool,
    first_prompt_cache_key: Option<String>,
    previous_response_id: Option<String>,
    previous_call_ids: BTreeSet<String>,
}

pub(super) struct LiveApiTurn {
    pub(super) phase: String,
    pub(super) request: serde_json::Value,
    pub(super) response: Option<serde_json::Value>,
    polling: Option<DetectedPollingTurn>,
    previous_response_id: Option<String>,
    previous_call_ids: BTreeSet<String>,
    replayed_call_ids: BTreeSet<String>,
    pub(super) response_events: Vec<serde_json::Value>,
}

pub(super) struct LiveApiNotice {
    kind: &'static str,
    summary: String,
}

#[derive(Serialize)]
pub(super) struct ProgressRecord {
    schema_version: u32,
    sequence: u64,
    observed_at: DateTime<Utc>,
    elapsed_ms: u64,
    arm: &'static str,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    coordinate: Option<ProgressCoordinate>,
}

impl DiffProgress {
    pub(super) async fn start(
        path: PathBuf,
        started: Instant,
    ) -> InternalResult<(Self, DiffProgressRecorder)> {
        Self::start_with_heartbeat(path, started, PROGRESS_HEARTBEAT_INTERVAL).await
    }

    pub(super) async fn start_with_heartbeat(
        path: PathBuf,
        started: Instant,
        heartbeat_interval: Duration,
    ) -> InternalResult<(Self, DiffProgressRecorder)> {
        let mut output = tokio::fs::File::create(&path)
            .await
            .wrap_err_with(|| format!("failed to create live progress log {}", path.display()))?;
        let progress_path = path.clone();
        let (sender, mut receiver) = mpsc::unbounded_channel::<PendingProgressRecord>();
        let task = tokio::spawn(async move {
            let mut sequence = 0_u64;
            let mut lanes = BTreeMap::new();
            let mut heartbeat = tokio::time::interval(heartbeat_interval);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            heartbeat.tick().await;
            loop {
                tokio::select! {
                    biased;
                    pending = receiver.recv() => {
                        let Some(pending) = pending else {
                            break;
                        };
                        if matches!(pending.arm, "nanocodex" | "codex") {
                            lanes.insert(
                                pending.arm,
                                LaneProgressState {
                                    elapsed_ms: pending.elapsed_ms,
                                    kind: pending.kind.clone(),
                                    summary: pending.summary.clone(),
                                },
                            );
                        }
                        sequence = sequence.saturating_add(1);
                        write_progress_record(&mut output, &progress_path, sequence, pending)
                            .await?;
                    }
                    _ = heartbeat.tick(), if heartbeat_needed(&lanes) => {
                        let elapsed_ms = elapsed_ms(started);
                        sequence = sequence.saturating_add(1);
                        write_progress_record(
                            &mut output,
                            &progress_path,
                            sequence,
                            PendingProgressRecord {
                                observed_at: Utc::now(),
                                elapsed_ms,
                                arm: "runner",
                                kind: "heartbeat".to_owned(),
                                summary: Some(heartbeat_summary(&lanes, elapsed_ms)),
                                coordinate: None,
                            },
                        )
                        .await?;
                    }
                }
            }
            output.sync_all().await
        });
        Ok((
            Self {
                active: Some(ActiveDiffProgress {
                    sender,
                    started,
                    api_diff: Arc::new(Mutex::new(LiveApiDiff::default())),
                }),
            },
            DiffProgressRecorder { path, task },
        ))
    }

    pub(super) fn emit(
        &self,
        arm: &'static str,
        kind: impl Into<String>,
        summary: impl Into<String>,
    ) {
        let summary = summary.into();
        self.emit_record(arm, kind.into(), summary, None);
    }

    pub(super) fn emit_comparison_started(
        &self,
        task: &Task,
        profile: DifferentialProfile,
        trial: usize,
        summary: impl Into<String>,
    ) {
        self.emit_record(
            "runner",
            "comparison.started".to_owned(),
            summary.into(),
            Some(ProgressCoordinate {
                task_name: task.name().to_owned(),
                task_content_digest: task.content_digest().to_owned(),
                thinking: profile.thinking.as_str().to_owned(),
                nanocodex_tool_mode: profile.nanocodex_tool_mode.as_str().to_owned(),
                codex_tool_mode: profile.codex_tool_mode.as_str().to_owned(),
                trial,
            }),
        );
    }

    fn emit_record(
        &self,
        arm: &'static str,
        kind: String,
        summary: String,
        coordinate: Option<ProgressCoordinate>,
    ) {
        let Some(active) = &self.active else {
            return;
        };
        let _ = active.sender.send(PendingProgressRecord {
            observed_at: Utc::now(),
            elapsed_ms: elapsed_ms(active.started),
            arm,
            kind,
            summary: (!summary.is_empty()).then_some(summary),
            coordinate,
        });
    }

    pub(super) fn observe_nanocodex(&self, event: &nanocodex_agent::events::AgentEvent) {
        let kind = serde_json::to_value(event.kind)
            .ok()
            .and_then(|kind| kind.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", event.kind));
        let payload = serde_json::from_str(event.payload.get()).unwrap_or_default();
        self.emit(
            "nanocodex",
            kind,
            summarize_nanocodex(&event.kind, &payload),
        );
    }

    pub(super) fn observe_evaluator(&self, arm: &'static str, event: &EvalEventKind) {
        match event {
            EvalEventKind::VerifierStarted => {
                self.emit(arm, "verifier.started", "canonical verifier");
            }
            EvalEventKind::VerifierOutput { stdout, stderr } => {
                self.emit(
                    arm,
                    "verifier.output",
                    format!(
                        "{} stdout bytes · {} stderr bytes",
                        stdout.len(),
                        stderr.len()
                    ),
                );
            }
            EvalEventKind::VerifierCompleted(result) => {
                let rewards = result
                    .rewards
                    .iter()
                    .map(|(name, reward)| format!("{name}={reward}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.emit(
                    arm,
                    "verifier.completed",
                    format!("exit {} · {rewards}", result.exit_code),
                );
            }
            EvalEventKind::AttemptStarted { .. }
            | EvalEventKind::Agent(_)
            | EvalEventKind::Completed(_)
            | EvalEventKind::Failed(_)
            | EvalEventKind::RunCompleted
            | EvalEventKind::RunFailed { .. } => {}
        }
    }

    pub(super) fn observe_nanocodex_api(&self, payload: &serde_json::Value) {
        let direction = payload
            .get("direction")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let phase = payload
            .get("phase")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let request_index = payload
            .get("model_call_index")
            .and_then(serde_json::Value::as_u64);
        let event = payload.get("event").unwrap_or(&serde_json::Value::Null);
        self.observe_live_api("nanocodex", direction, phase, request_index, event);
        self.emit_api_boundary("nanocodex", direction, phase, request_index, event);
    }

    pub(super) fn observe_api_exchange(&self, arm: &'static str, exchange: &serde_json::Value) {
        let direction = exchange
            .get("direction")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let phase = exchange
            .get("phase")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let request_index = exchange
            .get("request_index")
            .and_then(serde_json::Value::as_u64);
        let payload = exchange.get("payload").unwrap_or(&serde_json::Value::Null);
        let event = payload
            .get("event")
            .or_else(|| payload.get("text"))
            .unwrap_or(payload);
        for event in record_api_events(exchange) {
            self.observe_live_api(arm, direction, phase, request_index, &event);
        }
        self.emit_api_boundary(arm, direction, phase, request_index, event);
    }

    fn observe_live_api(
        &self,
        arm: &'static str,
        direction: &str,
        phase: &str,
        request_index: Option<u64>,
        event: &serde_json::Value,
    ) {
        let Some(active) = &self.active else {
            return;
        };
        let notices = {
            let Ok(mut diff) = active.api_diff.lock() else {
                return;
            };
            diff.observe(arm, direction, phase, request_index, event)
        };
        for notice in notices {
            self.emit("runner", notice.kind, notice.summary);
        }
    }

    fn emit_api_boundary(
        &self,
        arm: &'static str,
        direction: &str,
        phase: &str,
        request_index: Option<u64>,
        event: &serde_json::Value,
    ) {
        let event_type = api_event_type(event);
        let terminal = matches!(
            event_type.as_deref(),
            Some("response.completed" | "response.failed" | "error")
        );
        if direction != "outbound" && !terminal {
            return;
        }
        let kind = if direction == "outbound" {
            "api.request".to_owned()
        } else {
            format!("api.{}", event_type.as_deref().unwrap_or("response"))
        };
        self.emit(
            arm,
            kind,
            summarize_api_boundary(phase, request_index, event_type.as_deref(), event),
        );
    }

    pub(super) fn observe_codex(&self, event: &serde_json::Value) {
        let kind = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        self.emit("codex", kind, summarize_codex(event));
    }

    pub(super) fn observe_codex_diagnostic(&self, diagnostic: &[u8]) {
        self.emit(
            "codex",
            "diagnostic",
            preview(&String::from_utf8_lossy(diagnostic)),
        );
    }
}

impl LiveApiDiff {
    fn observe(
        &mut self,
        arm: &'static str,
        direction: &str,
        phase: &str,
        request_index: Option<u64>,
        event: &serde_json::Value,
    ) -> Vec<LiveApiNotice> {
        if !matches!(arm, "nanocodex" | "codex") {
            return Vec::new();
        }
        let changed =
            self.arms
                .entry(arm)
                .or_default()
                .observe(direction, phase, request_index, event);
        if !changed {
            return Vec::new();
        }
        let mut notices = Vec::new();
        let (Some(nanocodex), Some(codex)) = (self.arms.get("nanocodex"), self.arms.get("codex"))
        else {
            return notices;
        };
        let aligned = nanocodex.turns.len().min(codex.turns.len());
        while self.compared_requests < aligned {
            let offset = self.compared_requests;
            let request_number = offset.saturating_add(1);
            let nanocodex_request = serde_json::json!({
                "phase": nanocodex.turns[offset].phase,
                "request": nanocodex.turns[offset].request,
            });
            let codex_request = serde_json::json!({
                "phase": codex.turns[offset].phase,
                "request": codex.turns[offset].request,
            });
            let mut differences = Vec::new();
            diff_json(
                "",
                Some(&nanocodex_request),
                Some(&codex_request),
                &mut differences,
            );
            notices.push(live_api_notice(request_number, "request", &differences));
            self.compared_requests = self.compared_requests.saturating_add(1);
        }
        while self.compared_responses < aligned {
            let offset = self.compared_responses;
            let (Some(nanocodex_response), Some(codex_response)) = (
                nanocodex.turns[offset].response.as_ref(),
                codex.turns[offset].response.as_ref(),
            ) else {
                break;
            };
            let request_number = offset.saturating_add(1);
            let nanocodex_response = serde_json::json!({
                "response": nanocodex_response,
            });
            let codex_response = serde_json::json!({
                "response": codex_response,
            });
            let mut differences = Vec::new();
            diff_json(
                "",
                Some(&nanocodex_response),
                Some(&codex_response),
                &mut differences,
            );
            notices.push(live_api_notice(request_number, "response", &differences));
            let nanocodex_polling = nanocodex.turns[offset].polling.as_ref();
            let codex_polling = codex.turns[offset].polling.as_ref();
            if nanocodex_polling.is_some() || codex_polling.is_some() {
                let shape = |polling: Option<&DetectedPollingTurn>| {
                    polling.map(|polling| {
                        (
                            polling.empty_stdin_calls,
                            polling.calls_with_explicit_yield,
                            polling.explicit_requested_yield_ms,
                        )
                    })
                };
                let format_polling = |polling: Option<&DetectedPollingTurn>| {
                    polling.map_or_else(
                        || "none".to_owned(),
                        |polling| {
                            format!(
                                "{} calls/{} explicit/{}ms",
                                polling.empty_stdin_calls,
                                polling.calls_with_explicit_yield,
                                polling.explicit_requested_yield_ms,
                            )
                        },
                    )
                };
                let matches = shape(nanocodex_polling) == shape(codex_polling);
                notices.push(LiveApiNotice {
                    kind: if matches {
                        "api.polling.match"
                    } else {
                        "api.polling.diff"
                    },
                    summary: format!(
                        "turn {request_number} poll-only response · nanocodex={} · codex={}",
                        format_polling(nanocodex_polling),
                        format_polling(codex_polling),
                    ),
                });
            }
            self.compared_responses = self.compared_responses.saturating_add(1);
        }
        notices
    }
}

impl LiveApiArm {
    pub(super) fn observe(
        &mut self,
        direction: &str,
        phase: &str,
        request_index: Option<u64>,
        event: &serde_json::Value,
    ) -> bool {
        if direction == "outbound" && api_event_type(event).as_deref() == Some("response.create") {
            let offset = self.turns.len();
            if !self.initial_request_seen {
                self.first_prompt_cache_key = event
                    .get("prompt_cache_key")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                self.initial_request_seen = true;
            }
            let replayed_call_ids = request_call_ids(event);
            let context = EventLoopNormalizeContext {
                stage: EventLoopValueStage::Request,
                first_prompt_cache_key: self.first_prompt_cache_key.as_deref(),
                previous_response_id: self.previous_response_id.as_deref(),
                previous_call_ids: &self.previous_call_ids,
                replayed_call_ids: &replayed_call_ids,
            };
            self.turns.push(LiveApiTurn {
                phase: phase.to_owned(),
                request: normalize_event_loop_value(event, None, &context),
                response: None,
                polling: None,
                previous_response_id: self.previous_response_id.clone(),
                previous_call_ids: self.previous_call_ids.clone(),
                replayed_call_ids,
                response_events: Vec::new(),
            });
            if let Some(request_index) = request_index {
                self.source_offsets.insert(request_index, offset);
            }
            self.active_offset = Some(offset);
            true
        } else if direction == "inbound"
            && let Some(offset) = request_index
                .and_then(|request_index| self.source_offsets.get(&request_index).copied())
                .or(self.active_offset)
            && let Some(turn) = self.turns.get_mut(offset)
        {
            let kind = api_event_type(event);
            if kind.as_deref().is_some_and(is_semantic_response_event) {
                turn.response_events.push(event.clone());
            }
            if !kind.as_deref().is_some_and(is_terminal_api_event) || turn.response.is_some() {
                return false;
            }
            let context = EventLoopNormalizeContext {
                stage: EventLoopValueStage::Response,
                first_prompt_cache_key: self.first_prompt_cache_key.as_deref(),
                previous_response_id: turn.previous_response_id.as_deref(),
                previous_call_ids: &turn.previous_call_ids,
                replayed_call_ids: &turn.replayed_call_ids,
            };
            turn.response = Some(event_loop_response_signature(
                &turn.response_events,
                &context,
            ));
            turn.polling = detected_polling_turn(&turn.response_events);
            self.previous_response_id = response_id(&turn.response_events);
            self.previous_call_ids = response_call_ids(&turn.response_events);
            turn.response_events.clear();
            true
        } else {
            false
        }
    }
}

pub(super) fn live_api_notice(
    request_number: usize,
    stage: &'static str,
    differences: &[ApiJsonDifference],
) -> LiveApiNotice {
    if differences.is_empty() {
        return LiveApiNotice {
            kind: if stage == "request" {
                "api.request.match"
            } else {
                "api.response.match"
            },
            summary: format!("turn {request_number} {stage} invariants match"),
        };
    }
    let categories = event_loop_difference_categories(differences);
    let pointer = differences
        .first()
        .map_or("", |difference| difference.pointer.as_str());
    LiveApiNotice {
        kind: if stage == "request" {
            "api.request.diff"
        } else {
            "api.response.diff"
        },
        summary: format!(
            "turn {request_number} {stage} drift · {} · {pointer}",
            categories.join(",")
        ),
    }
}

impl DiffProgressRecorder {
    pub(super) async fn finish(self, progress: DiffProgress) -> InternalResult<()> {
        drop(progress);
        self.task
            .await
            .wrap_err("live progress recorder task failed")?
            .wrap_err_with(|| format!("failed to write live progress log {}", self.path.display()))
    }
}

pub(super) async fn write_progress_record(
    output: &mut tokio::fs::File,
    path: &Path,
    sequence: u64,
    pending: PendingProgressRecord,
) -> std::io::Result<()> {
    let record = ProgressRecord {
        schema_version: PROGRESS_SCHEMA_VERSION,
        sequence,
        observed_at: pending.observed_at,
        elapsed_ms: pending.elapsed_ms,
        arm: pending.arm,
        kind: pending.kind,
        summary: pending.summary,
        coordinate: pending.coordinate,
    };
    let mut encoded = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    tracing::info!(
        target: "nanocodex_eval::diff_progress",
        elapsed_ms = record.elapsed_ms,
        comparison_arm = record.arm,
        event_kind = %record.kind,
        summary = record.summary.as_deref().unwrap_or(""),
        progress_path = %path.display(),
        "differential progress"
    );
    Ok(())
}

pub(super) fn heartbeat_needed(lanes: &BTreeMap<&'static str, LaneProgressState>) -> bool {
    lanes.is_empty()
        || ["nanocodex", "codex"].into_iter().any(|arm| {
            lanes.get(arm).is_none_or(|lane| {
                !matches!(lane.kind.as_str(), "attempt.completed" | "attempt.failed")
            })
        })
}

pub(super) fn heartbeat_summary(
    lanes: &BTreeMap<&'static str, LaneProgressState>,
    elapsed_ms: u64,
) -> String {
    ["nanocodex", "codex"]
        .into_iter()
        .map(|arm| {
            lanes.get(arm).map_or_else(
                || format!("{arm}: not started"),
                |lane| {
                    let state = lane.summary.as_deref().map_or_else(
                        || lane.kind.clone(),
                        |summary| {
                            format!(
                                "{} ({})",
                                lane.kind,
                                preview_chars(summary, PROGRESS_HEARTBEAT_SUMMARY_CHARS)
                            )
                        },
                    );
                    format!(
                        "{arm}: {state} for {}",
                        format_duration(elapsed_ms.saturating_sub(lane.elapsed_ms))
                    )
                },
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(super) fn format_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        format!("{duration_ms}ms")
    } else {
        format!("{:.1}s", duration_ms as f64 / 1_000.0)
    }
}

pub(super) fn summarize_nanocodex(kind: &AgentEventKind, payload: &serde_json::Value) -> String {
    match kind {
        AgentEventKind::AssistantMessage => value_preview(payload, "text"),
        AgentEventKind::ToolCall => join_summary([
            value_string(payload, "tool"),
            value_preview_option(payload, "arguments"),
        ]),
        AgentEventKind::ToolResult => join_summary([
            value_string(payload, "tool"),
            value_string(payload, "status"),
            value_preview_option(payload, "result"),
        ]),
        AgentEventKind::ModelCallStarted
        | AgentEventKind::ModelCallCompleted
        | AgentEventKind::ModelCallFailed => join_summary([
            labeled_value(payload, "call", "call_index"),
            value_string(payload, "status"),
            labeled_value(payload, "tools", "tool_calls"),
            value_preview_option(payload, "error"),
        ]),
        AgentEventKind::ModelAttemptFailed => join_summary([
            labeled_value(payload, "call", "model_call_index"),
            labeled_value(payload, "attempt", "attempt"),
            labeled_value(payload, "max", "max_attempts"),
            value_string(payload, "failure_phase").map(|phase| format!("phase {phase}")),
            value_string(payload, "error_class").map(|class| format!("class {class}")),
            labeled_value(payload, "retryable", "retryable"),
            labeled_value(payload, "billing uncertain", "billing_uncertain"),
            value_preview_option(payload, "error"),
        ]),
        AgentEventKind::ModelAttemptRetrying => join_summary([
            labeled_value(payload, "call", "model_call_index"),
            labeled_value(payload, "attempt", "attempt"),
            labeled_value(payload, "next", "next_attempt"),
            labeled_value(payload, "max", "max_attempts"),
            value_string(payload, "failure_phase").map(|phase| format!("phase {phase}")),
            value_string(payload, "error_class").map(|class| format!("class {class}")),
            labeled_duration_ns(payload, "delay", "delay_ns"),
            labeled_value(payload, "new socket", "opens_new_socket"),
            value_string(payload, "replay_mode").map(|mode| format!("replay {mode}")),
            value_preview_option(payload, "error"),
        ]),
        AgentEventKind::ModelConnectionFailed => join_summary([
            value_string(payload, "transport"),
            labeled_value(payload, "attempt", "attempt"),
            value_string(payload, "purpose").map(|purpose| format!("purpose {purpose}")),
            value_preview_option(payload, "error"),
        ]),
        AgentEventKind::RunError | AgentEventKind::RunFailed => value_preview(payload, "message"),
        AgentEventKind::RunCompleted => join_summary([
            labeled_value(payload, "model calls", "model_calls"),
            labeled_value(payload, "tools", "tool_calls"),
        ]),
        _ => String::new(),
    }
}

pub(super) fn summarize_codex(event: &serde_json::Value) -> String {
    let kind = event
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    match kind {
        "thread.started" => labeled_value(event, "thread", "thread_id").unwrap_or_default(),
        "turn.completed" => event
            .get("usage")
            .map(|usage| format!("usage {}", preview_json(usage)))
            .unwrap_or_default(),
        "turn.failed" => event.get("error").map(preview_json).unwrap_or_default(),
        "item.started" | "item.updated" | "item.completed" => {
            summarize_codex_item(event.get("item"))
        }
        _ => String::new(),
    }
}

pub(super) fn summarize_codex_item(item: Option<&serde_json::Value>) -> String {
    let Some(item) = item else {
        return String::new();
    };
    let item_kind = item
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown item");
    let detail = match item_kind {
        "agent_message" | "reasoning" => value_preview(item, "text"),
        "command_execution" => join_summary([
            value_preview_option(item, "command"),
            labeled_value(item, "exit", "exit_code"),
            value_string(item, "status"),
        ]),
        "file_change" => join_summary([
            item.get("changes").map(preview_json),
            value_string(item, "status"),
        ]),
        "mcp_tool_call" => join_summary([
            value_string(item, "server"),
            value_string(item, "tool"),
            value_preview_option(item, "arguments"),
            value_string(item, "status"),
        ]),
        "web_search" => join_summary([
            value_preview_option(item, "query"),
            value_string(item, "status"),
        ]),
        _ => value_string(item, "status").unwrap_or_default(),
    };
    join_summary([
        Some(item_kind.to_owned()),
        (!detail.is_empty()).then_some(detail),
    ])
}

pub(super) fn api_event_type(event: &serde_json::Value) -> Option<String> {
    if let Some(event_type) = event.get("type").and_then(serde_json::Value::as_str) {
        return Some(event_type.to_owned());
    }
    let text = event.as_str()?;
    for line in text.lines() {
        let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(data)
            && let Some(event_type) = event.get("type").and_then(serde_json::Value::as_str)
        {
            return Some(event_type.to_owned());
        }
    }
    [
        "response.completed",
        "response.failed",
        "response.created",
        "error",
    ]
    .into_iter()
    .find(|event_type| text.contains(event_type))
    .map(str::to_owned)
}

pub(super) fn summarize_api_boundary(
    phase: &str,
    request_index: Option<u64>,
    event_type: Option<&str>,
    event: &serde_json::Value,
) -> String {
    let request = request_index.map(|index| format!("request {index}"));
    let event_type = event_type.map(str::to_owned);
    let model = value_string(event, "model").map(|model| format!("model {model}"));
    let input = event
        .get("input")
        .and_then(serde_json::Value::as_array)
        .map(|input| format!("input {}", input.len()));
    let tools = event
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .map(|tools| format!("tools {}", tools.len()));
    let previous = event.get("previous_response_id").map(|previous| {
        if previous.is_null() {
            "previous none".to_owned()
        } else {
            "previous set".to_owned()
        }
    });
    let usage = event
        .pointer("/response/usage")
        .or_else(|| event.get("usage"))
        .map(|usage| format!("usage {}", preview_json(usage)));
    let raw = event.as_str().map(preview).filter(|text| !text.is_empty());
    join_summary([
        request,
        Some(phase.to_owned()),
        event_type,
        model,
        input,
        tools,
        previous,
        usage,
        raw,
    ])
}

pub(super) fn labeled_value(value: &serde_json::Value, label: &str, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| (!value.is_null()).then(|| format!("{label} {}", preview_json(value))))
}

pub(super) fn labeled_duration_ns(
    value: &serde_json::Value,
    label: &str,
    key: &str,
) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .map(|ns| {
            let rounded_ms = ns.saturating_add(500_000) / 1_000_000;
            format!("{label} {}", format_duration(rounded_ms))
        })
}

pub(super) fn value_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| (!value.is_null()).then(|| preview_json(value)))
    })
}

pub(super) fn value_preview(value: &serde_json::Value, key: &str) -> String {
    value_preview_option(value, key).unwrap_or_default()
}

pub(super) fn value_preview_option(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|value| {
        if value.is_null() {
            None
        } else if let Some(text) = value.as_str() {
            Some(preview(text))
        } else {
            Some(preview_json(value))
        }
    })
}

pub(super) fn preview_json(value: &serde_json::Value) -> String {
    preview(&value.to_string())
}

pub(super) fn preview(text: &str) -> String {
    preview_chars(text, PROGRESS_SUMMARY_CHARS)
}

pub(super) fn preview_chars(text: &str, limit: usize) -> String {
    let mut normalized = String::with_capacity(text.len().min(limit));
    let mut whitespace = false;
    let mut truncated = false;
    let mut characters = 0_usize;
    for character in text.chars() {
        if character.is_whitespace() {
            whitespace = true;
            continue;
        }
        if whitespace && !normalized.is_empty() {
            if characters >= limit {
                truncated = true;
                break;
            }
            normalized.push(' ');
            characters = characters.saturating_add(1);
        }
        whitespace = false;
        if characters >= limit {
            truncated = true;
            break;
        }
        normalized.push(character);
        characters = characters.saturating_add(1);
    }
    if truncated {
        normalized.push('…');
    }
    normalized
}

pub(super) fn join_summary<const N: usize>(parts: [Option<String>; N]) -> String {
    parts
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}
