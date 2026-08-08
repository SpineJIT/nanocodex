use codex_spine_core::{MemorySlot, NodeStatus};
use nanocodex_spine_runtime::{SpineRuntime, SpineRuntimeLimits};
use pretty_assertions::assert_eq;

#[test]
fn close_replaces_a_live_child_with_its_compact_memory() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());

    let child = runtime.open("open-1", "inspect the parser").unwrap();
    let handoff = runtime
        .close("close-1", "parser needs a single-token lookahead")
        .unwrap();
    let projection = runtime.projection().unwrap();

    assert_eq!(child.to_string(), "1.1");
    assert_eq!(handoff.closed_node.to_string(), "1.1");
    assert_eq!(projection.cursor.to_string(), "1");
    assert_eq!(projection.nodes[1].status, NodeStatus::Closed);
    assert!(matches!(
        projection.nodes[1].memory.as_deref(),
        Some([MemorySlot::Summary { body, .. }]) if body == "parser needs a single-token lookahead"
    ));
}

#[test]
fn next_closes_the_current_child_and_starts_a_sibling() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());

    runtime.open("open-1", "inspect the parser").unwrap();
    let handoff = runtime
        .next(
            "next-1",
            "implement the fix",
            "the parser needs a single-token lookahead",
        )
        .unwrap();
    let projection = runtime.projection().unwrap();

    assert_eq!(handoff.closed_node.to_string(), "1.1");
    assert_eq!(handoff.live_node.to_string(), "1.2");
    assert_eq!(projection.cursor.to_string(), "1.2");
    assert_eq!(projection.nodes[1].status, NodeStatus::Closed);
    assert_eq!(projection.nodes[2].status, NodeStatus::Live);
}

#[test]
fn open_refuses_to_exceed_the_configured_depth() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits {
        max_depth: 1,
        max_nodes: 8,
        ..SpineRuntimeLimits::default()
    });

    runtime.open("open-1", "inspect the parser").unwrap();
    let error = runtime.open("open-2", "inspect the lexer").unwrap_err();

    assert_eq!(
        error.to_string(),
        "spine continuation depth limit of 1 reached"
    );
}

#[test]
fn rejected_node_limit_preserves_the_live_scope() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits {
        max_nodes: 1,
        ..SpineRuntimeLimits::default()
    });
    runtime.open("open-1", "inspect the parser").unwrap();

    let error = runtime.open("open-2", "inspect the lexer").unwrap_err();
    let projection = runtime.projection().unwrap();

    assert_eq!(
        error.to_string(),
        "spine continuation node limit of 1 reached"
    );
    assert_eq!(projection.cursor.to_string(), "1.1");
    assert_eq!(projection.nodes.len(), 2);
    assert_eq!(projection.nodes[1].status, NodeStatus::Live);
}

#[test]
fn one_code_mode_cell_cannot_emit_two_spine_handoffs() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());
    runtime
        .open("call-exec/code-1", "inspect the parser")
        .unwrap();
    runtime.close("close-1", "parser handoff").unwrap();

    let error = runtime
        .open("call-exec/code-2", "inspect the lexer")
        .unwrap_err();
    let projection = runtime.projection().unwrap();

    assert_eq!(
        error.to_string(),
        "spine__open may emit only one handoff per Code Mode cell"
    );
    assert_eq!(projection.cursor.to_string(), "1");
    assert_eq!(projection.nodes.len(), 2);
    assert_eq!(projection.nodes[1].status, NodeStatus::Closed);
}

#[test]
fn rejected_next_node_limit_preserves_the_live_scope() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits {
        max_nodes: 1,
        ..SpineRuntimeLimits::default()
    });
    runtime.open("open-1", "inspect the parser").unwrap();

    let error = runtime
        .next(
            "next-1",
            "implement the fix",
            "parser needs one-token lookahead",
        )
        .unwrap_err();
    let projection = runtime.projection().unwrap();

    assert_eq!(
        error.to_string(),
        "spine continuation node limit of 1 reached"
    );
    assert_eq!(projection.cursor.to_string(), "1.1");
    assert_eq!(projection.nodes.len(), 2);
    assert_eq!(projection.nodes[1].status, NodeStatus::Live);
    assert!(projection.nodes[1].memory.is_none());
}

#[test]
fn close_refuses_memory_that_would_overflow_the_next_scope_context() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits {
        max_memory_bytes: 8,
        ..SpineRuntimeLimits::default()
    });
    runtime.open("open-1", "inspect the parser").unwrap();

    let error = runtime.close("close-1", "too much memory").unwrap_err();

    assert_eq!(error.to_string(), "spine memory exceeds the 8-byte limit");
}

#[test]
fn summary_respects_the_hard_context_bound_when_configuration_is_larger() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits {
        max_summary_bytes: usize::MAX,
        ..SpineRuntimeLimits::default()
    });

    let error = runtime.open("open-1", &"x".repeat(1_001)).unwrap_err();

    assert_eq!(
        error.to_string(),
        "spine summary exceeds the 1000-byte limit"
    );
}

#[test]
fn memory_respects_the_hard_context_bound_when_configuration_is_larger() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits {
        max_memory_bytes: usize::MAX,
        ..SpineRuntimeLimits::default()
    });
    runtime.open("open-1", "inspect the parser").unwrap();

    let error = runtime.close("close-1", &"x".repeat(901)).unwrap_err();

    assert_eq!(error.to_string(), "spine memory exceeds the 900-byte limit");
}

#[test]
fn handoff_memory_leaves_room_for_its_model_visible_wrapper() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits {
        max_memory_bytes: usize::MAX,
        ..SpineRuntimeLimits::default()
    });
    runtime.open("open-1", "inspect the parser").unwrap();

    let error = runtime.close("close-1", &"x".repeat(901)).unwrap_err();

    assert_eq!(error.to_string(), "spine memory exceeds the 900-byte limit");
}

#[test]
fn rendered_continuation_context_never_exceeds_one_thousand_bytes() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits {
        max_summary_bytes: usize::MAX,
        ..SpineRuntimeLimits::default()
    });

    let error = runtime.open("open-1", &"x".repeat(1_000)).unwrap_err();

    assert_eq!(
        error.to_string(),
        "spine continuation context exceeds the 1000-byte limit"
    );
}

#[test]
fn open_rejects_a_summary_that_overflows_the_final_continuation_context() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());

    let error = runtime.open("open-1", &"x".repeat(999)).unwrap_err();
    let projection = runtime.projection().unwrap();

    assert_eq!(
        error.to_string(),
        "spine continuation context exceeds the 1000-byte limit"
    );
    assert_eq!(projection.cursor.to_string(), "1");
    assert_eq!(projection.nodes.len(), 1);
}

#[test]
fn next_rejects_an_oversized_handoff_without_closing_the_live_scope() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());
    runtime.open("open-1", "inspect the parser").unwrap();

    let error = runtime
        .next("next-1", "continue", &"y".repeat(900))
        .unwrap_err();
    let projection = runtime.projection().unwrap();

    assert_eq!(
        error.to_string(),
        "spine continuation context exceeds the 1000-byte limit"
    );
    assert_eq!(projection.cursor.to_string(), "1.1");
    assert_eq!(projection.nodes.len(), 2);
    assert_eq!(projection.nodes[1].status, NodeStatus::Live);
}
