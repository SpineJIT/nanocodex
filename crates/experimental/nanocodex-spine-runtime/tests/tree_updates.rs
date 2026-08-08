use std::sync::{Arc, Mutex};

use nanocodex_spine_runtime::{SpineRuntime, SpineRuntimeLimits, SpineTreeNodeStatus};
use pretty_assertions::assert_eq;

#[test]
fn observer_receives_open_next_and_close_tree_snapshots() {
    let runtime = SpineRuntime::new(SpineRuntimeLimits::default());
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&snapshots);
    runtime
        .set_tree_observer(Arc::new(move |snapshot| {
            received.lock().unwrap().push(snapshot);
        }))
        .unwrap();

    runtime.open("open-1", "inspect the parser").unwrap();
    runtime
        .next(
            "next-1",
            "implement the fix",
            "parser needs one-token lookahead",
        )
        .unwrap();
    runtime
        .close("close-1", "implementation is ready for review")
        .unwrap();

    let snapshots = snapshots.lock().unwrap();
    assert_eq!(snapshots.len(), 4);
    assert_eq!(snapshots[1].active_node_id, "1.1");
    assert_eq!(
        snapshots[1].nodes[1].summary.as_deref(),
        Some("inspect the parser")
    );
    assert_eq!(snapshots[1].nodes[1].status, SpineTreeNodeStatus::Live);
    assert_eq!(snapshots[2].active_node_id, "1.2");
    assert_eq!(snapshots[2].nodes[1].status, SpineTreeNodeStatus::Closed);
    assert_eq!(
        snapshots[2].nodes[2].summary.as_deref(),
        Some("implement the fix")
    );
    assert_eq!(snapshots[2].nodes[2].status, SpineTreeNodeStatus::Live);
    assert_eq!(snapshots[3].active_node_id, "1");
    assert_eq!(snapshots[3].nodes[2].status, SpineTreeNodeStatus::Closed);
}
