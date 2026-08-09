use codex_spine_core::NodeStatus;
use nanocodex::{TerminalToolReceipt, oai::tools::ToolOutputBody};
use nanocodex_spine_runtime::{
    SpineIntentRequest, SpineRuntime, SpineRuntimeLimits, SpineTerminalControl, SpineTransitionKind,
};
use pretty_assertions::assert_eq;
use serde_json::{json, value::to_raw_value};
use tempfile::tempdir;

#[test]
fn committed_open_and_close_restore_the_durable_parent_tree() {
    let directory = tempdir().expect("temporary journal directory");
    let runtime = fresh_runtime(directory.path());

    let open = runtime
        .prepare(SpineIntentRequest::new(
            "root-session",
            "call-open",
            SpineTerminalControl::Open {
                summary: "inspect the parser".to_owned(),
            },
        ))
        .expect("prepare open");
    let open_delivery = runtime
        .commit(&open, "child-session", None, "delivery-open")
        .expect("commit open");
    assert_eq!(open_delivery.target_session_id(), "child-session");

    let close = runtime
        .prepare(SpineIntentRequest::new(
            "child-session",
            "call-close",
            SpineTerminalControl::Close {
                memory: "the parser needs one-token lookahead".to_owned(),
            },
        ))
        .expect("prepare close");
    assert_eq!(close.kind(), SpineTransitionKind::Close);
    runtime
        .commit(
            &close,
            "root-session",
            Some("child-session".to_owned()),
            "delivery-close",
        )
        .expect("commit close");

    let projection = runtime.projection().expect("projection");
    assert_eq!(projection.cursor.to_string(), "1");
    assert_eq!(projection.nodes[1].status, NodeStatus::Closed);
    assert_eq!(runtime.active_session_id().expect("active"), "root-session");
}

#[test]
fn terminal_receipt_requires_the_same_prepared_open_control() {
    let directory = tempdir().expect("temporary journal directory");
    let runtime = fresh_runtime(directory.path());
    runtime
        .prepare(SpineIntentRequest::new(
            "root-session",
            "call-open/code-1",
            SpineTerminalControl::Open {
                summary: "inspect the parser".to_owned(),
            },
        ))
        .expect("prepare open");

    let transition = runtime
        .transition_for_receipt(
            "root-session",
            &receipt(
                "call-open/code-1",
                "spine__open",
                json!({"kind": "open", "summary": "inspect the parser"}),
            ),
        )
        .expect("match prepared receipt");
    assert_eq!(transition.kind(), SpineTransitionKind::Open);

    let error = runtime
        .transition_for_receipt(
            "root-session",
            &receipt(
                "call-open/code-1",
                "spine__open",
                json!({"kind": "open", "summary": "inspect a different parser"}),
            ),
        )
        .expect_err("mismatched receipt must not commit");
    assert_eq!(
        error.to_string(),
        "spine terminal receipt does not match its prepared control"
    );
}

#[test]
fn final_delivery_prompt_is_bounded_before_the_transition_is_prepared() {
    let directory = tempdir().expect("temporary journal directory");
    let runtime = SpineRuntime::create(
        SpineRuntimeLimits {
            max_summary_bytes: usize::MAX,
            ..SpineRuntimeLimits::default()
        },
        directory.path(),
        "root-session",
        "root-cache-key",
        "2026-08-09T00:00:00Z",
    )
    .expect("create runtime");

    let error = runtime
        .prepare(SpineIntentRequest::new(
            "root-session",
            "call-open",
            SpineTerminalControl::Open {
                summary: "x".repeat(1_000),
            },
        ))
        .expect_err("final context must stay bounded");

    assert_eq!(
        error.to_string(),
        "spine continuation context exceeds the 1000-byte limit"
    );
    assert!(
        runtime
            .pending_transition()
            .expect("read pending")
            .is_none()
    );
}

#[test]
fn a_parked_session_cannot_prepare_the_next_spine_transition() {
    let directory = tempdir().expect("temporary journal directory");
    let runtime = fresh_runtime(directory.path());
    let open = runtime
        .prepare(SpineIntentRequest::new(
            "root-session",
            "call-open",
            SpineTerminalControl::Open {
                summary: "inspect the parser".to_owned(),
            },
        ))
        .expect("prepare open");
    runtime
        .commit(&open, "child-session", None, "delivery-open")
        .expect("commit open");

    let error = runtime
        .prepare(SpineIntentRequest::new(
            "root-session",
            "call-parked-open",
            SpineTerminalControl::Open {
                summary: "incorrectly resume the parent".to_owned(),
            },
        ))
        .expect_err("only the active session can prepare a Spine transition");

    assert_eq!(
        error.to_string(),
        "spine transition must originate from the active session"
    );
    assert!(
        runtime
            .pending_transition()
            .expect("read pending")
            .is_none()
    );
}

#[test]
fn reopening_the_sidecar_selects_the_durable_active_child_and_pending_delivery() {
    let directory = tempdir().expect("temporary journal directory");
    let runtime = fresh_runtime(directory.path());
    let open = runtime
        .prepare(SpineIntentRequest::new(
            "root-session",
            "call-open",
            SpineTerminalControl::Open {
                summary: "inspect the parser".to_owned(),
            },
        ))
        .expect("prepare open");
    runtime
        .commit(&open, "child-session", None, "delivery-open")
        .expect("commit open");
    drop(runtime);

    let reopened = SpineRuntime::open(
        SpineRuntimeLimits::default(),
        directory.path(),
        "root-session",
    )
    .expect("reopen runtime");

    assert_eq!(
        reopened.active_session_id().expect("active session"),
        "child-session"
    );
    let delivery = reopened
        .unclaimed_active_delivery()
        .expect("read pending delivery")
        .expect("open delivery remains pending");
    assert_eq!(delivery.id(), "delivery-open");
    assert_eq!(delivery.target_session_id(), "child-session");
}

fn fresh_runtime(directory: &std::path::Path) -> SpineRuntime {
    SpineRuntime::create(
        SpineRuntimeLimits::default(),
        directory,
        "root-session",
        "root-cache-key",
        "2026-08-09T00:00:00Z",
    )
    .expect("create runtime")
}

fn receipt(call_id: &str, tool_name: &str, metadata: serde_json::Value) -> TerminalToolReceipt {
    TerminalToolReceipt::new(
        call_id.to_owned(),
        tool_name.to_owned(),
        ToolOutputBody::Text(r#"{"accepted":true}"#.to_owned()),
        Some(to_raw_value(&metadata).expect("metadata")),
    )
    .expect("receipt")
}
