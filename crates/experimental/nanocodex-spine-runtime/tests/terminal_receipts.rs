use codex_spine_core::NodeStatus;
use nanocodex::TerminalToolReceipt;
use nanocodex::oai::tools::ToolOutputBody;
use nanocodex_spine_runtime::{SpineRuntime, SpineRuntimeLimits, SpineTerminalTransition};
use pretty_assertions::assert_eq;
use serde_json::{json, value::to_raw_value};

#[test]
fn close_terminal_receipt_commits_the_child_memory() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());
    runtime.open("open-1", "inspect the parser").unwrap();

    let transition = runtime
        .accept_terminal_receipt(&receipt(
            "close-1",
            "spine__close",
            json!({ "kind": "close", "memory": "parser needs one-token lookahead" }),
        ))
        .unwrap();
    let projection = runtime.projection().unwrap();

    assert!(matches!(transition, SpineTerminalTransition::Closed { .. }));
    assert_eq!(projection.cursor.to_string(), "1");
    assert_eq!(projection.nodes[1].status, NodeStatus::Closed);
}

#[test]
fn next_terminal_receipt_returns_the_sibling_handoff() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());
    runtime.open("open-1", "inspect the parser").unwrap();

    let transition = runtime
        .accept_terminal_receipt(&receipt(
            "next-1",
            "spine__next",
            json!({
                "kind": "next",
                "summary": "implement the fix",
                "memory": "parser needs one-token lookahead"
            }),
        ))
        .unwrap();

    assert!(matches!(
        transition,
        SpineTerminalTransition::Next { summary, memory, .. }
            if summary == "implement the fix" && memory == "parser needs one-token lookahead"
    ));
    assert_eq!(runtime.projection().unwrap().cursor.to_string(), "1.2");
}

#[test]
fn terminal_receipt_rejects_a_mismatched_control_payload() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());
    runtime.open("open-1", "inspect the parser").unwrap();

    let error = runtime
        .accept_terminal_receipt(&receipt(
            "close-1",
            "spine__close",
            json!({
                "kind": "next",
                "summary": "implement the fix",
                "memory": "parser needs one-token lookahead"
            }),
        ))
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "spine terminal receipt does not match its tool name"
    );
}

#[test]
fn terminal_receipt_rejects_missing_or_malformed_control_metadata() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());
    runtime.open("open-1", "inspect the parser").unwrap();

    let missing = runtime
        .accept_terminal_receipt(
            &TerminalToolReceipt::new(
                "close-1".to_owned(),
                "spine__close".to_owned(),
                ToolOutputBody::Text(r#"{"accepted":true}"#.to_owned()),
                None,
            )
            .unwrap(),
        )
        .unwrap_err();
    let malformed = runtime
        .accept_terminal_receipt(&receipt("close-2", "spine__close", json!("not a control")))
        .unwrap_err();
    let projection = runtime.projection().unwrap();

    assert_eq!(
        missing.to_string(),
        "spine terminal receipt has no valid control metadata"
    );
    assert_eq!(
        malformed.to_string(),
        "spine terminal receipt has no valid control metadata"
    );
    assert_eq!(projection.cursor.to_string(), "1.1");
    assert_eq!(projection.nodes[1].status, NodeStatus::Live);
}

fn receipt(call_id: &str, tool_name: &str, metadata: serde_json::Value) -> TerminalToolReceipt {
    TerminalToolReceipt::new(
        call_id.to_owned(),
        tool_name.to_owned(),
        ToolOutputBody::Text(r#"{"accepted":true}"#.to_owned()),
        Some(to_raw_value(&metadata).unwrap()),
    )
    .unwrap()
}
