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
fn close_refuses_memory_that_would_overflow_the_next_scope_context() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits {
        max_memory_bytes: 8,
        ..SpineRuntimeLimits::default()
    });
    runtime.open("open-1", "inspect the parser").unwrap();

    let error = runtime.close("close-1", "too much memory").unwrap_err();

    assert_eq!(error.to_string(), "spine memory exceeds the 8-byte limit");
}
