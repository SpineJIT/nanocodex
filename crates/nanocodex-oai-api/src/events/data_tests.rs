use serde_json::json;

use super::RunTerminal;

#[test]
fn legacy_run_terminal_without_completion_remains_readable() {
    let terminal: RunTerminal = serde_json::from_value(json!({
        "status": "completed",
        "model": "gpt-5",
        "reasoning_mode": "native",
        "effort": "high",
        "transport": "websocket",
        "orchestration": "single_agent",
        "duration_ms": 1,
        "duration_ns": 1,
        "model_calls": 1,
        "steers": 0,
        "compactions": 0,
        "tool_calls": 0,
        "connection_attempts": 1,
        "websocket_reconnects": 0,
        "response_attempts": 1,
        "response_retries": 0,
        "billing_uncertain_response_attempts": 0,
        "connection_duration_ns": 0,
        "retry_backoff_duration_ns": 0,
        "model_duration_ns": 1,
        "compaction_duration_ns": 0,
        "warmup_duration_ns": 0,
        "tool_work_duration_ns": 0,
        "tool_wall_duration_ns": 0,
        "usage": {
            "input_tokens": 0,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "total_tokens": 0
        },
        "warmup_usage": {
            "input_tokens": 0,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "total_tokens": 0
        },
        "estimated_cost": null,
        "cost_usd": null
    }))
    .expect("legacy terminal payload should remain readable");

    assert!(terminal.completion.is_none());
}
