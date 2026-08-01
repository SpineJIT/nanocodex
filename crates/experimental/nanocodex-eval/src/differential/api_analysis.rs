use super::*;

pub(super) fn retain_arm_api_exchanges(
    event_log: &Path,
    attempt_directory: &Path,
    codex_arm: bool,
    required: bool,
) -> InternalResult<Option<ApiCaptureArtifact>> {
    let path = attempt_directory.join(API_EXCHANGES_FILE);
    if codex_arm {
        if !path.is_file() {
            if required {
                return Err(diff_error!(
                    "stock Codex retained no API exchange capture at {}",
                    path.display()
                ));
            }
            return Ok(None);
        }
        return Ok(Some(inspect_api_exchanges(
            path,
            "all_api_payloads_routed_through_configured_base_url",
            "exact_wire_payload_bytes",
        )?));
    }
    project_nanocodex_api_exchanges(event_log, &path)?;
    Ok(Some(inspect_api_exchanges(
        path,
        "responses_request_and_response_payloads",
        "complete_observed_json_values",
    )?))
}

pub(super) fn project_nanocodex_api_exchanges(
    event_log: &Path,
    output: &Path,
) -> InternalResult<()> {
    let parent = output
        .parent()
        .ok_or_else(|| diff_error!("API exchange path has no parent: {}", output.display()))?;
    fs::create_dir_all(parent)?;
    let input =
        BufReader::new(File::open(event_log).wrap_err_with(|| {
            format!("failed to open evaluator event log {}", event_log.display())
        })?);
    let mut output_file = File::create(output)
        .wrap_err_with(|| format!("failed to create API exchange log {}", output.display()))?;
    let mut sequence = 0_u64;
    let mut request_index = 0_u64;
    for (line_index, line) in input.lines().enumerate() {
        let line = line.wrap_err_with(|| {
            format!(
                "failed to read evaluator event line {} from {}",
                line_index.saturating_add(1),
                event_log.display()
            )
        })?;
        let envelope: serde_json::Value = serde_json::from_str(&line).wrap_err_with(|| {
            format!(
                "invalid evaluator event JSON at {}:{}",
                event_log.display(),
                line_index.saturating_add(1)
            )
        })?;
        if envelope.get("type").and_then(serde_json::Value::as_str) != Some("agent")
            || envelope
                .pointer("/payload/type")
                .and_then(serde_json::Value::as_str)
                != Some("api.event")
        {
            continue;
        }
        let api = envelope
            .pointer("/payload/payload")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                diff_error!(
                    "Nanocodex API event has no object payload at {}:{}",
                    event_log.display(),
                    line_index.saturating_add(1)
                )
            })?;
        let direction = api
            .get("direction")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        if direction == "outbound" {
            request_index = request_index.saturating_add(1);
        }
        let event = api.get("event").cloned().unwrap_or(serde_json::Value::Null);
        let payload_bytes = serde_json::to_vec(&event)?.len();
        sequence = sequence.saturating_add(1);
        let record = serde_json::json!({
            "schema_version": API_CAPTURE_SCHEMA_VERSION,
            "sequence": sequence,
            "direction": direction,
            "transport": api
                .get("transport")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown"),
            "request_index": request_index,
            "model_call_index": api.get("model_call_index"),
            "phase": api
                .get("phase")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown"),
            "kind": "message",
            "payload_bytes": payload_bytes,
            "payload": {
                "encoding": "json",
                "event": event,
            },
        });
        serde_json::to_writer(&mut output_file, &record)?;
        output_file.write_all(b"\n")?;
    }
    output_file.flush()?;
    output_file.sync_all()?;
    Ok(())
}

pub(super) fn inspect_api_exchanges(
    path: PathBuf,
    payload_scope: &'static str,
    payload_fidelity: &'static str,
) -> InternalResult<ApiCaptureArtifact> {
    let input = BufReader::new(
        File::open(&path)
            .wrap_err_with(|| format!("failed to open API exchange log {}", path.display()))?,
    );
    let mut summary = ApiCaptureSummary {
        schema_version: API_CAPTURE_SCHEMA_VERSION,
        payload_scope,
        header_scope: "forwarded_not_retained",
        payload_fidelity,
        records: 0,
        requests: 0,
        response_requests: 0,
        auxiliary_requests: 0,
        inbound_events: 0,
        terminal_events: 0,
        http_responses_completed: 0,
        payload_bytes: 0,
        exchange_complete: false,
        transports: BTreeMap::new(),
        phases: BTreeMap::new(),
    };
    let mut outbound_requests = BTreeSet::new();
    let mut response_requests = BTreeSet::new();
    let mut http_requests = BTreeSet::new();
    let mut terminal_response_requests = BTreeSet::new();
    let mut completed_http_requests = BTreeSet::new();
    for (line_index, line) in input.lines().enumerate() {
        let line = line?;
        let record: serde_json::Value = serde_json::from_str(&line).wrap_err_with(|| {
            format!(
                "invalid API exchange JSON at {}:{}",
                path.display(),
                line_index.saturating_add(1)
            )
        })?;
        summary.records = summary.records.saturating_add(1);
        summary.payload_bytes = summary.payload_bytes.saturating_add(
            record
                .get("payload_bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
        );
        if let Some(transport) = record.get("transport").and_then(serde_json::Value::as_str) {
            increment(&mut summary.transports, transport);
        }
        if let Some(phase) = record.get("phase").and_then(serde_json::Value::as_str) {
            increment(&mut summary.phases, phase);
        }
        let request_index = record
            .get("request_index")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                diff_error!(
                    "API exchange has no request index at {}:{}",
                    path.display(),
                    line_index.saturating_add(1)
                )
            })?;
        match record.get("direction").and_then(serde_json::Value::as_str) {
            Some("outbound") => {
                outbound_requests.insert(request_index);
                if record_api_event_type(&record).as_deref() == Some("response.create") {
                    response_requests.insert(request_index);
                }
                if record.get("transport").and_then(serde_json::Value::as_str)
                    == Some("responses_https")
                {
                    http_requests.insert(request_index);
                }
            }
            Some("inbound") => {
                summary.inbound_events = summary.inbound_events.saturating_add(1);
                if record_api_event_type(&record).is_some_and(|event_type| {
                    matches!(
                        event_type.as_str(),
                        "response.completed" | "response.failed" | "error"
                    )
                }) {
                    summary.terminal_events = summary.terminal_events.saturating_add(1);
                    terminal_response_requests.insert(request_index);
                }
                if record.get("kind").and_then(serde_json::Value::as_str)
                    == Some("response_completed")
                {
                    completed_http_requests.insert(request_index);
                }
            }
            _ => {}
        }
    }
    summary.requests = u64::try_from(outbound_requests.len()).unwrap_or(u64::MAX);
    summary.response_requests = u64::try_from(response_requests.len()).unwrap_or(u64::MAX);
    summary.auxiliary_requests = summary.requests.saturating_sub(summary.response_requests);
    summary.http_responses_completed =
        u64::try_from(completed_http_requests.len()).unwrap_or(u64::MAX);
    summary.exchange_complete = !outbound_requests.is_empty()
        && outbound_requests.iter().all(|request_index| {
            (response_requests.contains(request_index) || http_requests.contains(request_index))
                && (!response_requests.contains(request_index)
                    || terminal_response_requests.contains(request_index))
                && (!http_requests.contains(request_index)
                    || completed_http_requests.contains(request_index))
        });
    Ok(ApiCaptureArtifact { path, summary })
}

pub(super) fn increment(counts: &mut BTreeMap<String, u64>, key: &str) {
    let count = counts.entry(key.to_owned()).or_default();
    *count = count.saturating_add(1);
}

pub(super) fn record_api_event_type(record: &serde_json::Value) -> Option<String> {
    record_api_events(record)
        .into_iter()
        .find_map(|event| api_event_type(&event))
        .or_else(|| {
            let payload = record.get("payload")?;
            api_event_type(
                payload
                    .get("event")
                    .or_else(|| payload.get("text"))
                    .unwrap_or(payload),
            )
        })
}

pub(super) fn retain_api_comparison(
    path: &Path,
    nanocodex: &ArmReport,
    codex: &ArmReport,
) -> InternalResult<ApiComparisonSummary> {
    compare_api_exchanges(
        path,
        nanocodex.api_exchanges.as_deref(),
        codex.api_exchanges.as_deref(),
        nanocodex.api_capture.clone(),
        codex.api_capture.clone(),
    )
}

pub(super) fn validate_differential_profile(
    summary: &ApiComparisonSummary,
    expected_model: &str,
    expected_effort: &str,
    nanocodex_tool_mode: NanocodexToolMode,
    codex_tool_mode: CodexToolMode,
    web_search: bool,
) -> Option<String> {
    if !summary.comparable {
        return None;
    }
    let expected_nanocodex = expected_nanocodex_visible_tools(nanocodex_tool_mode, web_search);
    let expected_code_mode_only = ["exec", "wait"];
    let expected_codex_code_mode =
        expected_nanocodex_visible_tools(NanocodexToolMode::CodeMode, web_search);
    let nanocodex = summary.event_loop.nanocodex.as_ref()?;
    let codex = summary.event_loop.codex.as_ref()?;
    let base_matches = |arm: &ApiEventLoopArmSummary| {
        arm.initial_model.as_deref() == Some(expected_model)
            && arm.initial_reasoning_effort.as_deref() == Some(expected_effort)
            && arm.initial_reasoning_summary.as_deref() == Some("auto")
    };
    let visible_tools_match = |arm: &ApiEventLoopArmSummary, expected: &[&str]| {
        arm.initial_visible_tools
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied())
    };
    let nanocodex_matches =
        base_matches(nanocodex) && visible_tools_match(nanocodex, &expected_nanocodex);
    let codex_matches = base_matches(codex)
        && match codex_tool_mode {
            CodexToolMode::CodeModeOnly => visible_tools_match(codex, &expected_code_mode_only),
            CodexToolMode::CodeMode => visible_tools_match(codex, &expected_codex_code_mode),
        };
    let model_input_matches = summary.event_loop.initial_input_text_sections_equal == Some(true)
        && summary
            .event_loop
            .initial_generation_input_text_sections_equal
            == Some(true);
    let visible_tool_definitions_match = match (nanocodex_tool_mode, codex_tool_mode) {
        (NanocodexToolMode::CodeModeOnly, CodexToolMode::CodeModeOnly)
        | (NanocodexToolMode::CodeMode, CodexToolMode::CodeMode) => {
            summary.event_loop.initial_visible_tool_definitions_equal == Some(true)
                && summary
                    .event_loop
                    .initial_generation_visible_tool_definitions_equal
                    == Some(true)
        }
        _ => true,
    };
    let code_mode_catalog_matches = match (nanocodex_tool_mode, codex_tool_mode) {
        (NanocodexToolMode::CodeModeOnly, CodexToolMode::CodeModeOnly) => {
            summary.event_loop.initial_code_mode_tool_names_equal == Some(true)
                && summary.event_loop.initial_code_mode_tool_definitions_equal == Some(true)
        }
        _ => true,
    };
    if nanocodex_matches
        && codex_matches
        && model_input_matches
        && visible_tool_definitions_match
        && code_mode_catalog_matches
    {
        return None;
    }
    Some(format!(
        "expected Nanocodex {} and stock Codex {} to use model={expected_model}, effort={expected_effort}, reasoning.summary=auto, the pinned visible-tool surfaces, and identical initial input text and complete tool definitions when the tool modes match (plus identical nested definitions when both are Code Mode-only); nanocodex={}/{}/summary={}/[{}], codex={}/{}/summary={}/[{}], initial_input_text_equal={:?}, initial_generation_input_text_equal={:?}, initial_tool_definitions_equal={:?}, initial_generation_tool_definitions_equal={:?}, nested_tool_names_equal={:?}, nested_tool_definitions_equal={:?}",
        nanocodex_tool_mode.as_str(),
        codex_tool_mode.as_str(),
        nanocodex.initial_model.as_deref().unwrap_or("unobserved"),
        nanocodex
            .initial_reasoning_effort
            .as_deref()
            .unwrap_or("unobserved"),
        nanocodex
            .initial_reasoning_summary
            .as_deref()
            .unwrap_or("unobserved"),
        nanocodex.initial_visible_tools.join(", "),
        codex.initial_model.as_deref().unwrap_or("unobserved"),
        codex
            .initial_reasoning_effort
            .as_deref()
            .unwrap_or("unobserved"),
        codex
            .initial_reasoning_summary
            .as_deref()
            .unwrap_or("unobserved"),
        codex.initial_visible_tools.join(", "),
        summary.event_loop.initial_input_text_sections_equal,
        summary
            .event_loop
            .initial_generation_input_text_sections_equal,
        summary.event_loop.initial_visible_tool_definitions_equal,
        summary
            .event_loop
            .initial_generation_visible_tool_definitions_equal,
        summary.event_loop.initial_code_mode_tool_names_equal,
        summary.event_loop.initial_code_mode_tool_definitions_equal,
    ))
}

pub(super) fn expected_nanocodex_visible_tools(
    tool_mode: NanocodexToolMode,
    web_search: bool,
) -> Vec<&'static str> {
    if tool_mode == NanocodexToolMode::CodeModeOnly {
        return vec!["exec", "wait"];
    }
    let mut tools = vec![
        "exec",
        "wait",
        "exec_command",
        "write_stdin",
        "update_plan",
        "apply_patch",
        "view_image",
    ];
    if web_search {
        tools.push("web");
    }
    tools.push("image_gen");
    tools
}

pub(super) fn retained_nanocodex_tool_mode(
    comparison: &serde_json::Value,
) -> InternalResult<NanocodexToolMode> {
    match comparison
        .pointer("/policy/nanocodex_tool_mode")
        .and_then(serde_json::Value::as_str)
    {
        Some("code_mode") => Ok(NanocodexToolMode::CodeMode),
        Some("code_mode_only") => Ok(NanocodexToolMode::CodeModeOnly),
        Some(tool_mode) => Err(diff_error!(
            "retained comparison has unsupported Nanocodex tool mode {tool_mode:?}"
        )),
        None => Err(diff_error!(
            "retained comparison has no /policy/nanocodex_tool_mode"
        )),
    }
}

pub(super) fn retained_codex_tool_mode(
    comparison: &serde_json::Value,
) -> InternalResult<CodexToolMode> {
    match comparison
        .pointer("/policy/codex_tool_mode")
        .and_then(serde_json::Value::as_str)
    {
        Some("code_mode") => Ok(CodexToolMode::CodeMode),
        Some("code_mode_only") => Ok(CodexToolMode::CodeModeOnly),
        Some(tool_mode) => Err(diff_error!(
            "retained comparison has unsupported stock Codex tool mode {tool_mode:?}"
        )),
        None => Err(diff_error!(
            "retained comparison has no /policy/codex_tool_mode"
        )),
    }
}

pub(super) fn compare_api_exchanges(
    path: &Path,
    nanocodex_path: Option<&Path>,
    codex_path: Option<&Path>,
    nanocodex_capture: Option<ApiCaptureSummary>,
    codex_capture: Option<ApiCaptureSummary>,
) -> InternalResult<ApiComparisonSummary> {
    let nanocodex_requests = nanocodex_path.map(read_api_request_payloads).transpose()?;
    let codex_requests = codex_path.map(read_api_request_payloads).transpose()?;
    let comparable = nanocodex_requests.is_some() && codex_requests.is_some();
    let nanocodex_event_loop = nanocodex_requests.as_deref().map(build_event_loop_trace);
    let codex_event_loop = codex_requests.as_deref().map(build_event_loop_trace);
    let request_count_equal = nanocodex_requests
        .as_ref()
        .zip(codex_requests.as_ref())
        .map(|(nanocodex, codex)| nanocodex.len() == codex.len());
    let nanocodex_requests = nanocodex_requests.unwrap_or_default();
    let codex_requests = codex_requests.unwrap_or_default();
    let aligned_request_count = nanocodex_requests.len().min(codex_requests.len());
    let request_count = nanocodex_requests.len().max(codex_requests.len());
    let nanocodex_unpaired_request_count = nanocodex_requests
        .len()
        .saturating_sub(aligned_request_count);
    let codex_unpaired_request_count = codex_requests.len().saturating_sub(aligned_request_count);
    let mut requests = Vec::with_capacity(request_count);
    let mut first_divergence = None;
    let mut equal_requests = 0_u64;
    let mut differing_requests = 0_u64;
    let mut first_event_loop_divergence = None;
    let mut first_generation_divergence = None;
    let mut equal_event_loop_turns = 0_u64;
    let mut differing_event_loop_turns = 0_u64;
    for offset in 0..request_count {
        let nanocodex = nanocodex_requests.get(offset);
        let codex = codex_requests.get(offset);
        let nanocodex_event_loop_turn = nanocodex_event_loop
            .as_ref()
            .and_then(|trace| trace.turns.get(offset));
        let codex_event_loop_turn = codex_event_loop
            .as_ref()
            .and_then(|trace| trace.turns.get(offset));
        let request_index = u64::try_from(offset).unwrap_or(u64::MAX).saturating_add(1);
        let mut differences = Vec::new();
        match (nanocodex, codex) {
            (Some(nanocodex), Some(codex)) => diff_json(
                "",
                Some(&nanocodex.payload),
                Some(&codex.payload),
                &mut differences,
            ),
            (Some(nanocodex), None) => {
                diff_json("", Some(&nanocodex.payload), None, &mut differences)
            }
            (None, Some(codex)) => {
                diff_json("", None, Some(&codex.payload), &mut differences);
            }
            (None, None) => {}
        }
        let equal = differences.is_empty();
        if offset < aligned_request_count {
            if equal {
                equal_requests = equal_requests.saturating_add(1);
            } else {
                differing_requests = differing_requests.saturating_add(1);
            }
        }
        if !equal && first_divergence.is_none() {
            first_divergence = Some(ApiFirstDivergence {
                request_index,
                pointer: differences
                    .first()
                    .map_or_else(String::new, |difference| difference.pointer.clone()),
            });
        }
        let mut event_loop_differences = Vec::new();
        diff_json(
            "",
            nanocodex_event_loop_turn,
            codex_event_loop_turn,
            &mut event_loop_differences,
        );
        let event_loop_equal = event_loop_differences.is_empty();
        let event_loop_categories = event_loop_difference_categories(&event_loop_differences);
        if offset < aligned_request_count {
            if event_loop_equal {
                equal_event_loop_turns = equal_event_loop_turns.saturating_add(1);
            } else {
                differing_event_loop_turns = differing_event_loop_turns.saturating_add(1);
            }
        }
        if !event_loop_equal && first_event_loop_divergence.is_none() {
            first_event_loop_divergence = Some(ApiEventLoopFirstDivergence {
                request_index,
                pointer: event_loop_differences
                    .first()
                    .map_or_else(String::new, |difference| difference.pointer.clone()),
                categories: event_loop_categories.clone(),
            });
        }
        let generation_turn = [nanocodex, codex].into_iter().flatten().any(|request| {
            request.phase.as_deref() == Some("generation")
                || request
                    .payload
                    .get("generate")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
        });
        if !event_loop_equal && generation_turn && first_generation_divergence.is_none() {
            first_generation_divergence = Some(ApiEventLoopFirstDivergence {
                request_index,
                pointer: event_loop_differences
                    .first()
                    .map_or_else(String::new, |difference| difference.pointer.clone()),
                categories: event_loop_categories.clone(),
            });
        }
        requests.push(ApiRequestComparison {
            request_index,
            nanocodex_request_index: nanocodex.map(|request| request.request_index),
            codex_request_index: codex.map(|request| request.request_index),
            nanocodex_phase: nanocodex.and_then(|request| request.phase.clone()),
            codex_phase: codex.and_then(|request| request.phase.clone()),
            equal,
            nanocodex_sha256: nanocodex.map(|request| request.sha256.clone()),
            codex_sha256: codex.map(|request| request.sha256.clone()),
            differences,
            event_loop: ApiEventLoopTurnComparison {
                equal: event_loop_equal,
                categories: event_loop_categories,
                nanocodex: nanocodex_event_loop_turn.cloned(),
                codex: codex_event_loop_turn.cloned(),
                differences: event_loop_differences,
            },
        });
    }
    let chain_invariants_equal = nanocodex_event_loop
        .as_ref()
        .zip(codex_event_loop.as_ref())
        .map(|(nanocodex, codex)| nanocodex.summary.chain_invariants_equal(&codex.summary));
    let model_visible_tool_sequence_equal = nanocodex_event_loop
        .as_ref()
        .zip(codex_event_loop.as_ref())
        .map(|(nanocodex, codex)| {
            nanocodex.summary.model_visible_tool_sequence
                == codex.summary.model_visible_tool_sequence
        });
    let initial_input_text_sections_equal = nanocodex_event_loop
        .as_ref()
        .zip(codex_event_loop.as_ref())
        .map(|(nanocodex, codex)| {
            nanocodex.summary.initial_input_text_sections
                == codex.summary.initial_input_text_sections
        });
    let initial_generation_input_text_sections_equal = nanocodex_event_loop
        .as_ref()
        .zip(codex_event_loop.as_ref())
        .map(|(nanocodex, codex)| {
            nanocodex.summary.initial_generation_input_text_sections
                == codex.summary.initial_generation_input_text_sections
        });
    let initial_visible_tool_definitions_equal = nanocodex_event_loop
        .as_ref()
        .zip(codex_event_loop.as_ref())
        .map(|(nanocodex, codex)| {
            nanocodex.summary.initial_visible_tool_definitions
                == codex.summary.initial_visible_tool_definitions
        });
    let initial_generation_visible_tool_definitions_equal = nanocodex_event_loop
        .as_ref()
        .zip(codex_event_loop.as_ref())
        .map(|(nanocodex, codex)| {
            nanocodex
                .summary
                .initial_generation_visible_tool_definitions
                == codex.summary.initial_generation_visible_tool_definitions
        });
    let initial_code_mode_tool_names_equal = nanocodex_event_loop
        .as_ref()
        .zip(codex_event_loop.as_ref())
        .and_then(|(nanocodex, codex)| {
            nanocodex
                .summary
                .initial_code_mode_tools
                .as_ref()
                .zip(codex.summary.initial_code_mode_tools.as_ref())
                .map(|(nanocodex, codex)| nanocodex == codex)
        });
    let initial_code_mode_tool_definitions_equal = nanocodex_event_loop
        .as_ref()
        .zip(codex_event_loop.as_ref())
        .and_then(|(nanocodex, codex)| {
            nanocodex
                .summary
                .initial_code_mode_tool_definitions
                .as_ref()
                .zip(codex.summary.initial_code_mode_tool_definitions.as_ref())
                .map(|(nanocodex, codex)| nanocodex == codex)
        });
    let nanocodex_unpaired_tail = nanocodex_event_loop
        .as_ref()
        .map(|trace| trace.unpaired_tail(aligned_request_count));
    let codex_unpaired_tail = codex_event_loop
        .as_ref()
        .map(|trace| trace.unpaired_tail(aligned_request_count));
    let event_loop = ApiEventLoopComparison {
        comparable,
        request_count_equal,
        chain_invariants_equal,
        model_visible_tool_sequence_equal,
        initial_input_text_sections_equal,
        initial_generation_input_text_sections_equal,
        initial_visible_tool_definitions_equal,
        initial_generation_visible_tool_definitions_equal,
        initial_code_mode_tool_names_equal,
        initial_code_mode_tool_definitions_equal,
        aligned_turns: u64::try_from(aligned_request_count).unwrap_or(u64::MAX),
        nanocodex_unpaired_turns: u64::try_from(nanocodex_unpaired_request_count)
            .unwrap_or(u64::MAX),
        codex_unpaired_turns: u64::try_from(codex_unpaired_request_count).unwrap_or(u64::MAX),
        equal_turns: equal_event_loop_turns,
        differing_turns: differing_event_loop_turns,
        first_divergence: first_event_loop_divergence,
        first_generation_divergence,
        nanocodex_unpaired_tail,
        codex_unpaired_tail,
        nanocodex: nanocodex_event_loop.map(|trace| trace.summary),
        codex: codex_event_loop.map(|trace| trace.summary),
    };
    let summary = ApiComparisonSummary {
        comparable,
        request_count_equal,
        aligned_requests: u64::try_from(aligned_request_count).unwrap_or(u64::MAX),
        nanocodex_unpaired_requests: u64::try_from(nanocodex_unpaired_request_count)
            .unwrap_or(u64::MAX),
        codex_unpaired_requests: u64::try_from(codex_unpaired_request_count).unwrap_or(u64::MAX),
        equal_requests,
        differing_requests,
        first_divergence: first_divergence.clone(),
        event_loop: event_loop.clone(),
    };
    let report = ApiComparisonReport {
        schema_version: API_COMPARISON_SCHEMA_VERSION,
        comparable,
        request_count_equal,
        aligned_requests: summary.aligned_requests,
        nanocodex_unpaired_requests: summary.nanocodex_unpaired_requests,
        codex_unpaired_requests: summary.codex_unpaired_requests,
        equal_requests,
        differing_requests,
        nanocodex: nanocodex_capture,
        codex: codex_capture,
        first_divergence,
        event_loop,
        requests,
    };
    write_json_atomic(path, &report)?;
    Ok(summary)
}

impl ApiComparisonSummary {
    pub(super) const fn unavailable() -> Self {
        Self {
            comparable: false,
            request_count_equal: None,
            aligned_requests: 0,
            nanocodex_unpaired_requests: 0,
            codex_unpaired_requests: 0,
            equal_requests: 0,
            differing_requests: 0,
            first_divergence: None,
            event_loop: ApiEventLoopComparison::unavailable(),
        }
    }
}

impl ApiEventLoopComparison {
    const fn unavailable() -> Self {
        Self {
            comparable: false,
            request_count_equal: None,
            chain_invariants_equal: None,
            model_visible_tool_sequence_equal: None,
            initial_input_text_sections_equal: None,
            initial_generation_input_text_sections_equal: None,
            initial_visible_tool_definitions_equal: None,
            initial_generation_visible_tool_definitions_equal: None,
            initial_code_mode_tool_names_equal: None,
            initial_code_mode_tool_definitions_equal: None,
            aligned_turns: 0,
            nanocodex_unpaired_turns: 0,
            codex_unpaired_turns: 0,
            equal_turns: 0,
            differing_turns: 0,
            first_divergence: None,
            first_generation_divergence: None,
            nanocodex_unpaired_tail: None,
            codex_unpaired_tail: None,
            nanocodex: None,
            codex: None,
        }
    }
}

pub(super) fn read_api_request_payloads(path: &Path) -> InternalResult<Vec<ApiRequestPayload>> {
    let input = BufReader::new(File::open(path)?);
    let mut requests = BTreeMap::new();
    for (line_index, line) in input.lines().enumerate() {
        let line = line?;
        let record: serde_json::Value = serde_json::from_str(&line).wrap_err_with(|| {
            format!(
                "invalid API exchange JSON at {}:{}",
                path.display(),
                line_index.saturating_add(1)
            )
        })?;
        let request_index = record
            .get("request_index")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| {
                u64::try_from(requests.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(1)
            });
        match record.get("direction").and_then(serde_json::Value::as_str) {
            Some("outbound") => {
                let Some(payload) = record_api_events(&record).into_iter().next() else {
                    continue;
                };
                if api_event_type(&payload).as_deref() != Some("response.create") {
                    continue;
                }
                let encoded = serde_json::to_vec(&payload)?;
                requests.insert(
                    request_index,
                    ApiRequestPayload {
                        request_index,
                        phase: record
                            .get("phase")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        payload,
                        sha256: hex::encode(Sha256::digest(encoded)),
                        response_events: Vec::new(),
                    },
                );
            }
            Some("inbound") => {
                if let Some(request) = requests.get_mut(&request_index) {
                    request.response_events.extend(record_api_events(&record));
                }
            }
            _ => {}
        }
    }
    Ok(requests.into_values().collect())
}

pub(super) fn record_api_events(record: &serde_json::Value) -> Vec<serde_json::Value> {
    let Some(payload) = record.get("payload") else {
        return Vec::new();
    };
    if let Some(event) = payload.get("event") {
        return vec![event.clone()];
    }
    let Some(text) = payload.get("text").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    if let Ok(event) = serde_json::from_str(text) {
        return vec![event];
    }
    text.lines()
        .filter_map(|line| {
            let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
            (data != "[DONE]")
                .then(|| serde_json::from_str(data).ok())
                .flatten()
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(super) enum EventLoopValueStage {
    Request,
    Response,
}

pub(super) struct EventLoopNormalizeContext<'a> {
    stage: EventLoopValueStage,
    first_prompt_cache_key: Option<&'a str>,
    previous_response_id: Option<&'a str>,
    previous_call_ids: &'a BTreeSet<String>,
    replayed_call_ids: &'a BTreeSet<String>,
}

pub(super) fn build_event_loop_trace(requests: &[ApiRequestPayload]) -> ApiEventLoopTrace {
    let initial_generation_request = requests
        .iter()
        .find(|request| request.phase.as_deref() == Some("generation"))
        .or_else(|| {
            requests.iter().find(|request| {
                request
                    .payload
                    .get("generate")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
            })
        });
    let initial_input_text_sections = requests
        .first()
        .map_or_else(Vec::new, |request| input_text_sections(&request.payload));
    let initial_generation_input_text_sections = initial_generation_request
        .map_or_else(Vec::new, |request| input_text_sections(&request.payload));
    let initial_visible_tool_definitions = requests.first().map_or_else(Vec::new, |request| {
        visible_tool_definition_summaries(&request.payload)
    });
    let initial_generation_visible_tool_definitions = initial_generation_request
        .map_or_else(Vec::new, |request| {
            visible_tool_definition_summaries(&request.payload)
        });
    let initial_model = requests
        .first()
        .and_then(|request| request.payload.get("model"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let initial_reasoning_effort = requests
        .first()
        .and_then(|request| request.payload.pointer("/reasoning/effort"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let initial_reasoning_summary = requests
        .first()
        .and_then(|request| request.payload.pointer("/reasoning/summary"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let initial_visible_tools = requests
        .first()
        .map_or_else(Vec::new, |request| visible_tool_names(&request.payload));
    let initial_code_mode_tool_definitions = requests
        .first()
        .and_then(|request| code_mode_tool_definitions(&request.payload));
    let initial_code_mode_tools = initial_code_mode_tool_definitions
        .as_ref()
        .map(|definitions| {
            definitions
                .iter()
                .map(|definition| definition.name.clone())
                .collect()
        });
    let first_prompt_cache_key = requests
        .first()
        .and_then(|request| request.payload.get("prompt_cache_key"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let mut prompt_cache_key_stable = first_prompt_cache_key.as_ref().map(|_| true);
    let mut previous_response_id = None;
    let mut previous_call_ids = BTreeSet::new();
    let mut previous_response_links = 0_u64;
    let mut full_history_replays = 0_u64;
    let mut full_history_replays_after_nonterminal_turn = 0_u64;
    let mut broken_previous_response_links = 0_u64;
    let mut tool_result_links = 0_u64;
    let mut replayed_tool_result_links = 0_u64;
    let mut broken_tool_result_links = 0_u64;
    let mut generation_turns = 0_u64;
    let mut terminal_turns = 0_u64;
    let mut turns_with_usage = 0_u64;
    let mut turns_without_usage = 0_u64;
    let mut usage = ApiTokenUsageSummary::default();
    let mut tool_call_turns = 0_u64;
    let mut model_visible_tool_sequence = Vec::new();
    let mut detected_poll_only_turns = 0_u64;
    let mut consecutive_detected_poll_only_turns = 0_u64;
    let mut max_consecutive_detected_poll_only_turns = 0_u64;
    let mut detected_empty_stdin_calls = 0_u64;
    let mut detected_polling_calls_with_explicit_yield = 0_u64;
    let mut detected_polling_explicit_yield_ms = 0_u64;
    let mut detected_poll_only_input_tokens = 0_u64;
    let mut detected_poll_only_cached_tokens = 0_u64;
    let mut detected_poll_only_output_tokens = 0_u64;
    let mut turns = Vec::with_capacity(requests.len());
    let mut turn_metrics = Vec::with_capacity(requests.len());
    let mut previous_turn_terminal = false;

    for (offset, request) in requests.iter().enumerate() {
        let generation = request
            .payload
            .get("generate")
            .and_then(serde_json::Value::as_bool)
            != Some(false);
        if generation {
            generation_turns = generation_turns.saturating_add(1);
        }
        let prompt_cache_key = request
            .payload
            .get("prompt_cache_key")
            .and_then(serde_json::Value::as_str);
        if first_prompt_cache_key.is_some() && prompt_cache_key != first_prompt_cache_key.as_deref()
        {
            prompt_cache_key_stable = Some(false);
        }

        let request_previous_response_id = request
            .payload
            .get("previous_response_id")
            .and_then(serde_json::Value::as_str);
        let replayed_call_ids = request_call_ids(&request.payload);
        let full_history_replay =
            request_previous_response_id.is_none() && request_replays_history(&request.payload);
        if offset > 0 {
            if request_previous_response_id.is_some()
                && request_previous_response_id == previous_response_id.as_deref()
            {
                previous_response_links = previous_response_links.saturating_add(1);
            } else if full_history_replay {
                full_history_replays = full_history_replays.saturating_add(1);
                if !previous_turn_terminal {
                    full_history_replays_after_nonterminal_turn =
                        full_history_replays_after_nonterminal_turn.saturating_add(1);
                }
            } else {
                broken_previous_response_links = broken_previous_response_links.saturating_add(1);
            }
        }

        for call_id in request_tool_result_call_ids(&request.payload) {
            if previous_call_ids.contains(call_id) {
                tool_result_links = tool_result_links.saturating_add(1);
            } else if replayed_call_ids.contains(call_id) {
                tool_result_links = tool_result_links.saturating_add(1);
                replayed_tool_result_links = replayed_tool_result_links.saturating_add(1);
            } else {
                broken_tool_result_links = broken_tool_result_links.saturating_add(1);
            }
        }

        let request_context = EventLoopNormalizeContext {
            stage: EventLoopValueStage::Request,
            first_prompt_cache_key: first_prompt_cache_key.as_deref(),
            previous_response_id: previous_response_id.as_deref(),
            previous_call_ids: &previous_call_ids,
            replayed_call_ids: &replayed_call_ids,
        };
        let normalized_request =
            normalize_event_loop_value(&request.payload, None, &request_context);
        let response_context = EventLoopNormalizeContext {
            stage: EventLoopValueStage::Response,
            first_prompt_cache_key: first_prompt_cache_key.as_deref(),
            previous_response_id: previous_response_id.as_deref(),
            previous_call_ids: &previous_call_ids,
            replayed_call_ids: &replayed_call_ids,
        };
        let normalized_response =
            event_loop_response_signature(&request.response_events, &response_context);
        let response_tools = response_tool_items(&request.response_events)
            .filter_map(visible_tool_name)
            .collect::<Vec<_>>();
        let response_tool_count = u64::try_from(response_tools.len()).unwrap_or(u64::MAX);
        if response_tool_count > 0 {
            tool_call_turns = tool_call_turns.saturating_add(1);
        }
        model_visible_tool_sequence.extend(response_tools);
        let detected_polling = generation
            .then(|| detected_polling_turn(&request.response_events))
            .flatten();
        if let Some(polling) = &detected_polling {
            detected_poll_only_turns = detected_poll_only_turns.saturating_add(1);
            consecutive_detected_poll_only_turns =
                consecutive_detected_poll_only_turns.saturating_add(1);
            max_consecutive_detected_poll_only_turns =
                max_consecutive_detected_poll_only_turns.max(consecutive_detected_poll_only_turns);
            detected_empty_stdin_calls =
                detected_empty_stdin_calls.saturating_add(polling.empty_stdin_calls);
            detected_polling_calls_with_explicit_yield = detected_polling_calls_with_explicit_yield
                .saturating_add(polling.calls_with_explicit_yield);
            detected_polling_explicit_yield_ms = detected_polling_explicit_yield_ms
                .saturating_add(polling.explicit_requested_yield_ms);
            detected_poll_only_input_tokens =
                detected_poll_only_input_tokens.saturating_add(polling.input_tokens);
            detected_poll_only_cached_tokens =
                detected_poll_only_cached_tokens.saturating_add(polling.cached_tokens);
            detected_poll_only_output_tokens =
                detected_poll_only_output_tokens.saturating_add(polling.output_tokens);
        } else {
            consecutive_detected_poll_only_turns = 0;
        }
        let turn_terminal = request
            .response_events
            .iter()
            .any(|event| api_event_type(event).is_some_and(|kind| is_terminal_api_event(&kind)));
        if turn_terminal {
            terminal_turns = terminal_turns.saturating_add(1);
        }
        turns.push(serde_json::json!({
            "phase": request.phase,
            "request": normalized_request,
            "response": normalized_response,
        }));
        let turn_usage = api_response_usage(&request.response_events);
        if let Some(turn_usage) = &turn_usage {
            turns_with_usage = turns_with_usage.saturating_add(1);
            usage.add(turn_usage);
        } else {
            turns_without_usage = turns_without_usage.saturating_add(1);
        }
        turn_metrics.push(ApiEventLoopTurnMetrics {
            generation,
            tool_calls: response_tool_count,
            detected_polling,
            usage: turn_usage,
        });

        previous_response_id = response_id(&request.response_events);
        previous_call_ids = response_call_ids(&request.response_events);
        previous_turn_terminal = turn_terminal;
    }

    ApiEventLoopTrace {
        turns,
        turn_metrics,
        summary: ApiEventLoopArmSummary {
            turns: u64::try_from(requests.len()).unwrap_or(u64::MAX),
            generation_turns,
            terminal_turns,
            turns_with_usage,
            turns_without_usage,
            usage,
            tool_call_turns,
            model_visible_tool_calls: u64::try_from(model_visible_tool_sequence.len())
                .unwrap_or(u64::MAX),
            model_visible_tool_sequence,
            initial_model,
            initial_reasoning_effort,
            initial_reasoning_summary,
            initial_visible_tools,
            initial_input_text_sections,
            initial_generation_input_text_sections,
            initial_visible_tool_definitions,
            initial_generation_visible_tool_definitions,
            initial_code_mode_tools,
            initial_code_mode_tool_definitions,
            detected_poll_only_turns,
            max_consecutive_detected_poll_only_turns,
            detected_empty_stdin_calls,
            detected_polling_calls_with_explicit_yield,
            detected_polling_explicit_yield_ms,
            detected_poll_only_input_tokens,
            detected_poll_only_cached_tokens,
            detected_poll_only_output_tokens,
            prompt_cache_key_stable,
            previous_response_links,
            full_history_replays,
            full_history_replays_after_nonterminal_turn,
            broken_previous_response_links,
            tool_result_links,
            replayed_tool_result_links,
            broken_tool_result_links,
        },
    }
}

pub(super) fn input_text_sections(request: &serde_json::Value) -> Vec<ApiInputTextSectionSummary> {
    request
        .get("input")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .flat_map(|(item_ordinal, item)| {
            let role = item
                .get("role")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            item.get("content")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(move |(content_ordinal, content)| {
                    if content.get("type").and_then(serde_json::Value::as_str) != Some("input_text")
                    {
                        return None;
                    }
                    let text = content.get("text").and_then(serde_json::Value::as_str)?;
                    Some(ApiInputTextSectionSummary {
                        item_ordinal: u64::try_from(item_ordinal).unwrap_or(u64::MAX),
                        content_ordinal: u64::try_from(content_ordinal).unwrap_or(u64::MAX),
                        role: role.clone(),
                        label: input_text_label(text),
                        text_bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
                        text_sha256: hex::encode(Sha256::digest(text.as_bytes())),
                    })
                })
        })
        .collect()
}

pub(super) fn input_text_label(text: &str) -> String {
    let text = text.trim_start();
    if text.starts_with("# AGENTS.md instructions") {
        return "agents_md".to_owned();
    }
    if let Some(tag) = text.strip_prefix('<').and_then(|text| {
        let end = text.find(|character: char| character == '>' || character.is_whitespace())?;
        (end > 0).then(|| &text[..end])
    }) {
        return tag.to_owned();
    }
    "plain_text".to_owned()
}

pub(super) fn visible_tool_names(request: &serde_json::Value) -> Vec<String> {
    visible_tools(request)
        .filter_map(visible_tool_name)
        .collect::<Vec<_>>()
}

pub(super) fn visible_tools(
    request: &serde_json::Value,
) -> impl Iterator<Item = &serde_json::Value> {
    request
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            request
                .get("input")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter(|item| {
                    item.get("type").and_then(serde_json::Value::as_str) == Some("additional_tools")
                })
                .flat_map(|item| {
                    item.get("tools")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                }),
        )
}

pub(super) fn visible_tool_definition_summaries(
    request: &serde_json::Value,
) -> Vec<ApiVisibleToolDefinitionSummary> {
    visible_tools(request)
        .enumerate()
        .map(|(ordinal, definition)| {
            let description = definition
                .get("description")
                .and_then(serde_json::Value::as_str);
            let encoded = definition.to_string();
            ApiVisibleToolDefinitionSummary {
                name: visible_tool_name(definition).unwrap_or_else(|| "unnamed".to_owned()),
                ordinal: u64::try_from(ordinal).unwrap_or(u64::MAX),
                description_bytes: description
                    .map(|description| u64::try_from(description.len()).unwrap_or(u64::MAX)),
                description_sha256: description
                    .map(|description| hex::encode(Sha256::digest(description.as_bytes()))),
                definition_bytes: u64::try_from(encoded.len()).unwrap_or(u64::MAX),
                definition_sha256: hex::encode(Sha256::digest(encoded.as_bytes())),
            }
        })
        .collect()
}

pub(super) fn code_mode_tool_definitions(
    request: &serde_json::Value,
) -> Option<Vec<ApiCodeModeToolDefinitionSummary>> {
    let description = visible_tools(request)
        .find(|tool| visible_tool_name(tool).as_deref() == Some("exec"))?
        .get("description")
        .and_then(serde_json::Value::as_str)?;
    let mut definitions = Vec::new();
    let mut current_name = None::<String>;
    let mut current_section = String::new();

    for line in description.lines() {
        if let Some(name) = line
            .strip_prefix("### `")
            .and_then(|name| name.strip_suffix('`'))
        {
            if let Some(name) = current_name.take() {
                definitions.push(code_mode_tool_definition_summary(
                    name,
                    &current_section,
                    definitions.len(),
                ));
            }
            current_name = Some(name.to_owned());
            current_section.clear();
        }
        if current_name.is_some() {
            current_section.push_str(line);
            current_section.push('\n');
        }
    }
    if let Some(name) = current_name {
        definitions.push(code_mode_tool_definition_summary(
            name,
            &current_section,
            definitions.len(),
        ));
    }

    (!definitions.is_empty()).then_some(definitions)
}

pub(super) fn code_mode_tool_definition_summary(
    name: String,
    section: &str,
    ordinal: usize,
) -> ApiCodeModeToolDefinitionSummary {
    ApiCodeModeToolDefinitionSummary {
        name,
        ordinal: u64::try_from(ordinal).unwrap_or(u64::MAX),
        section_bytes: u64::try_from(section.len()).unwrap_or(u64::MAX),
        section_sha256: hex::encode(Sha256::digest(section.as_bytes())),
    }
}

pub(super) fn visible_tool_name(tool: &serde_json::Value) -> Option<String> {
    tool.get("name")
        .or_else(|| tool.get("type"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

pub(super) fn response_tool_items(
    events: &[serde_json::Value],
) -> impl Iterator<Item = &serde_json::Value> {
    events
        .iter()
        .filter(|event| api_event_type(event).as_deref() == Some("response.output_item.done"))
        .filter_map(|event| event.get("item"))
        .filter(|item| {
            item.get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind == "function_call" || kind.ends_with("_tool_call"))
        })
}

pub(super) fn api_response_usage(events: &[serde_json::Value]) -> Option<ApiTokenUsageSummary> {
    let usage = events
        .iter()
        .rev()
        .find_map(|event| event.pointer("/response/usage"))?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let cached_input_tokens = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let output_tokens = usage
        .get("output_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let reasoning_output_tokens = usage
        .pointer("/output_tokens_details/reasoning_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let total_tokens = usage
        .get("total_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    Some(ApiTokenUsageSummary {
        input_tokens,
        cached_input_tokens,
        uncached_input_tokens: input_tokens.saturating_sub(cached_input_tokens),
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
    })
}

pub(super) fn detected_polling_turn(events: &[serde_json::Value]) -> Option<DetectedPollingTurn> {
    let tool_items = response_tool_items(events).collect::<Vec<_>>();
    if tool_items.is_empty() {
        return None;
    }
    let empty_stdin_calls =
        tool_items
            .iter()
            .try_fold(DetectedEmptyStdinCalls::default(), |mut total, item| {
                let calls = detected_empty_stdin_calls(item)?;
                total.calls = total.calls.saturating_add(calls.calls);
                total.calls_with_explicit_yield = total
                    .calls_with_explicit_yield
                    .saturating_add(calls.calls_with_explicit_yield);
                total.explicit_requested_yield_ms = total
                    .explicit_requested_yield_ms
                    .saturating_add(calls.explicit_requested_yield_ms);
                Some(total)
            })?;
    let usage = api_response_usage(events).unwrap_or_default();
    Some(DetectedPollingTurn {
        empty_stdin_calls: empty_stdin_calls.calls,
        calls_with_explicit_yield: empty_stdin_calls.calls_with_explicit_yield,
        explicit_requested_yield_ms: empty_stdin_calls.explicit_requested_yield_ms,
        input_tokens: usage.input_tokens,
        cached_tokens: usage.cached_input_tokens,
        output_tokens: usage.output_tokens,
    })
}

pub(super) fn detected_empty_stdin_calls(
    item: &serde_json::Value,
) -> Option<DetectedEmptyStdinCalls> {
    let name = item.get("name").and_then(serde_json::Value::as_str)?;
    let kind = item.get("type").and_then(serde_json::Value::as_str)?;
    if kind == "function_call" && name == "write_stdin" {
        let arguments = item.get("arguments")?;
        let arguments = if let Some(arguments) = arguments.as_str() {
            serde_json::from_str(arguments).ok()?
        } else {
            arguments.clone()
        };
        return match arguments.get("chars") {
            None => Some(detected_direct_stdin_call(&arguments)),
            Some(serde_json::Value::String(chars)) if chars.is_empty() => {
                Some(detected_direct_stdin_call(&arguments))
            }
            _ => None,
        };
    }
    if kind != "custom_tool_call" || name != "exec" {
        return None;
    }
    let source = item.get("input").and_then(serde_json::Value::as_str)?;
    detected_code_mode_empty_stdin_calls(source)
}

pub(super) fn detected_direct_stdin_call(arguments: &serde_json::Value) -> DetectedEmptyStdinCalls {
    let explicit_requested_yield_ms = arguments
        .get("yield_time_ms")
        .and_then(serde_json::Value::as_u64);
    DetectedEmptyStdinCalls {
        calls: 1,
        calls_with_explicit_yield: u64::from(explicit_requested_yield_ms.is_some()),
        explicit_requested_yield_ms: explicit_requested_yield_ms.unwrap_or_default(),
    }
}

pub(super) fn detected_code_mode_empty_stdin_calls(
    source: &str,
) -> Option<DetectedEmptyStdinCalls> {
    let call_offsets = source
        .match_indices("tools.write_stdin")
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    if call_offsets.is_empty() || source.matches("tools.").count() != call_offsets.len() {
        return None;
    }
    let compact = source
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    if compact.contains("chars:")
        && !compact.contains("chars:\"\"")
        && !compact.contains("chars:''")
        && !compact.contains("\"chars\":\"\"")
        && !compact.contains("'chars':''")
    {
        return None;
    }
    let mut detected = DetectedEmptyStdinCalls {
        calls: u64::try_from(call_offsets.len()).unwrap_or(u64::MAX),
        ..DetectedEmptyStdinCalls::default()
    };
    for (index, offset) in call_offsets.iter().copied().enumerate() {
        let end = call_offsets.get(index + 1).copied().unwrap_or(source.len());
        if let Some(yield_ms) =
            explicit_u64_object_field(&source[offset..end], concat!("yield", "_", "time_ms"))
        {
            detected.calls_with_explicit_yield =
                detected.calls_with_explicit_yield.saturating_add(1);
            detected.explicit_requested_yield_ms = detected
                .explicit_requested_yield_ms
                .saturating_add(yield_ms);
        }
    }
    Some(detected)
}

pub(super) fn explicit_u64_object_field(source: &str, field: &str) -> Option<u64> {
    let (_, after_field) = source.split_once(field)?;
    let after_field = after_field.trim_start();
    let after_field = after_field
        .strip_prefix('"')
        .or_else(|| after_field.strip_prefix('\''))
        .unwrap_or(after_field)
        .trim_start();
    let digits = after_field
        .strip_prefix(':')?
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

pub(super) fn request_tool_result_call_ids(request: &serde_json::Value) -> Vec<&str> {
    request
        .get("input")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind.ends_with("_call_output"))
        })
        .filter_map(|item| item.get("call_id").and_then(serde_json::Value::as_str))
        .collect()
}

pub(super) fn request_call_ids(request: &serde_json::Value) -> BTreeSet<String> {
    request
        .get("input")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind.ends_with("_call") && !kind.ends_with("_call_output"))
        })
        .filter_map(|item| item.get("call_id").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect()
}

pub(super) fn request_replays_history(request: &serde_json::Value) -> bool {
    request
        .get("input")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|item| {
            item.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                || item
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| {
                        kind == "reasoning"
                            || (kind.ends_with("_call") && !kind.ends_with("_call_output"))
                    })
        })
}

pub(super) fn response_id(events: &[serde_json::Value]) -> Option<String> {
    events
        .iter()
        .filter_map(|event| {
            event
                .pointer("/response/id")
                .and_then(serde_json::Value::as_str)
        })
        .next_back()
        .map(str::to_owned)
}

pub(super) fn response_call_ids(events: &[serde_json::Value]) -> BTreeSet<String> {
    events
        .iter()
        .filter(|event| api_event_type(event).as_deref() == Some("response.output_item.done"))
        .filter_map(|event| {
            event
                .pointer("/item/call_id")
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned)
        .collect()
}

pub(super) fn event_loop_response_signature(
    events: &[serde_json::Value],
    context: &EventLoopNormalizeContext<'_>,
) -> serde_json::Value {
    let semantic_events = events
        .iter()
        .filter_map(api_event_type)
        .filter(|kind| is_semantic_response_event(kind))
        .collect::<Vec<_>>();
    let output_items = events
        .iter()
        .filter(|event| api_event_type(event).as_deref() == Some("response.output_item.done"))
        .filter_map(|event| event.get("item"))
        .map(|item| normalize_event_loop_value(item, None, context))
        .collect::<Vec<_>>();
    let terminal = events
        .iter()
        .rev()
        .find_map(|event| {
            let kind = api_event_type(event)?;
            is_terminal_api_event(&kind).then(|| {
                serde_json::json!({
                    "type": kind,
                    "status": event.pointer("/response/status"),
                    "error": event
                        .get("error")
                        .map(|error| normalize_event_loop_value(error, None, context)),
                })
            })
        })
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "semantic_events": semantic_events,
        "output_items": output_items,
        "terminal": terminal,
    })
}

pub(super) fn is_terminal_api_event(kind: &str) -> bool {
    matches!(kind, "response.completed" | "response.failed" | "error")
}

pub(super) fn is_semantic_response_event(kind: &str) -> bool {
    !kind.ends_with(".delta")
        && !kind.ends_with(".added")
        && !matches!(
            kind,
            "response.in_progress"
                | "codex.rate_limits"
                | "codex.response.metadata"
                | "responsesapi.websocket_timing"
        )
}

pub(super) fn normalize_event_loop_value(
    value: &serde_json::Value,
    key: Option<&str>,
    context: &EventLoopNormalizeContext<'_>,
) -> serde_json::Value {
    match key {
        Some("client_metadata") => return normalize_client_metadata(value),
        Some("prompt_cache_key") => {
            return serde_json::Value::String(value.as_str().map_or_else(
                || "missing".to_owned(),
                |key| {
                    if Some(key) == context.first_prompt_cache_key {
                        "stable".to_owned()
                    } else {
                        "changed".to_owned()
                    }
                },
            ));
        }
        Some("previous_response_id") => {
            return serde_json::Value::String(value.as_str().map_or_else(
                || "missing".to_owned(),
                |response_id| {
                    if Some(response_id) == context.previous_response_id {
                        "matches_previous_response".to_owned()
                    } else {
                        "present_unmatched".to_owned()
                    }
                },
            ));
        }
        Some("call_id") => {
            return serde_json::Value::String(match (context.stage, value.as_str()) {
                (EventLoopValueStage::Request, Some(call_id))
                    if context.previous_call_ids.contains(call_id) =>
                {
                    "matches_previous_output".to_owned()
                }
                (EventLoopValueStage::Request, Some(call_id))
                    if context.replayed_call_ids.contains(call_id) =>
                {
                    "matches_replayed_output".to_owned()
                }
                (EventLoopValueStage::Request, Some(_)) => "present_unmatched".to_owned(),
                (EventLoopValueStage::Response, Some(_)) => "present".to_owned(),
                (_, None) => "missing".to_owned(),
            });
        }
        Some(
            "text" | "description" | "instructions" | "arguments" | "encrypted_content"
            | "signature",
        ) if value.is_string() => return string_fingerprint(value.as_str().unwrap_or_default()),
        Some("input" | "output") if value.is_string() => {
            return string_fingerprint(value.as_str().unwrap_or_default());
        }
        _ => {}
    }

    match value {
        serde_json::Value::Object(object) => {
            let mut normalized = serde_json::Map::new();
            for (child_key, child) in object {
                if matches!(
                    child_key.as_str(),
                    "id" | "internal_chat_message_metadata_passthrough"
                ) {
                    continue;
                }
                normalized.insert(
                    child_key.clone(),
                    normalize_event_loop_value(child, Some(child_key), context),
                );
            }
            serde_json::Value::Object(normalized)
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| normalize_event_loop_value(value, None, context))
                .collect(),
        ),
        _ => value.clone(),
    }
}

pub(super) fn normalize_client_metadata(value: &serde_json::Value) -> serde_json::Value {
    let Some(metadata) = value.as_object() else {
        return value.clone();
    };
    let mut normalized = serde_json::Map::new();
    for (key, value) in metadata {
        if matches!(
            key.as_str(),
            "session_id"
                | "thread_id"
                | "turn_id"
                | "x-codex-installation-id"
                | "x-codex-turn-metadata"
                | "x-codex-window-id"
                | "x-codex-ws-stream-request-start-ms"
        ) {
            continue;
        }
        normalized.insert(key.clone(), value.clone());
    }
    serde_json::Value::Object(normalized)
}

pub(super) fn string_fingerprint(value: &str) -> serde_json::Value {
    serde_json::json!({
        "bytes": value.len(),
        "sha256": hex::encode(Sha256::digest(value.as_bytes())),
    })
}

pub(super) fn event_loop_difference_categories(differences: &[ApiJsonDifference]) -> Vec<String> {
    differences
        .iter()
        .map(|difference| event_loop_difference_category(&difference.pointer))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(super) fn event_loop_difference_category(pointer: &str) -> &'static str {
    if pointer.contains("/tools/") || pointer.ends_with("/tools") {
        "tool_configuration"
    } else if pointer.starts_with("/request/reasoning") {
        "reasoning_policy"
    } else if pointer.starts_with("/request/prompt_cache_key")
        || pointer.starts_with("/request/previous_response_id")
    {
        "response_chain"
    } else if pointer.starts_with("/request/input") {
        "request_context"
    } else if pointer.starts_with("/response/semantic_events") {
        "response_event_sequence"
    } else if pointer.starts_with("/response/output_items") {
        "model_output"
    } else if pointer.starts_with("/response/terminal") {
        "terminal_response"
    } else if pointer.starts_with("/phase") {
        "turn_alignment"
    } else if pointer.starts_with("/request") {
        "request_configuration"
    } else {
        "turn_presence"
    }
}

pub(super) fn diff_json(
    pointer: &str,
    nanocodex: Option<&serde_json::Value>,
    codex: Option<&serde_json::Value>,
    differences: &mut Vec<ApiJsonDifference>,
) {
    match (nanocodex, codex) {
        (Some(serde_json::Value::Object(nanocodex)), Some(serde_json::Value::Object(codex))) => {
            let keys = nanocodex
                .keys()
                .chain(codex.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            for key in keys {
                diff_json(
                    &json_pointer_child(pointer, &key),
                    nanocodex.get(&key),
                    codex.get(&key),
                    differences,
                );
            }
        }
        (Some(serde_json::Value::Array(nanocodex)), Some(serde_json::Value::Array(codex))) => {
            for index in 0..nanocodex.len().max(codex.len()) {
                diff_json(
                    &json_pointer_child(pointer, &index.to_string()),
                    nanocodex.get(index),
                    codex.get(index),
                    differences,
                );
            }
        }
        (Some(nanocodex), Some(codex)) if nanocodex == codex => {}
        (nanocodex, codex) => differences.push(ApiJsonDifference {
            pointer: pointer.to_owned(),
            nanocodex: nanocodex.map_or(ApiJsonSide::Missing, |value| ApiJsonSide::Value {
                value: value.clone(),
            }),
            codex: codex.map_or(ApiJsonSide::Missing, |value| ApiJsonSide::Value {
                value: value.clone(),
            }),
        }),
    }
}

pub(super) fn json_pointer_child(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}
