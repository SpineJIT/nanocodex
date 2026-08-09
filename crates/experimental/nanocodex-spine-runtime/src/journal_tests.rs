use std::{fs, io::Write};

use tempfile::tempdir;

use codex_spine_core::NodeStatus;

use crate::journal::{
    DeliveryKind, DeliveryStatus, Journal, JournalError, JournalHeader, TransitionIntent,
};

#[test]
fn journal_header_round_trips_and_selects_the_root_as_active() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new(
        "019c0d31-c308-7d91-bff4-5dca82d15ac6",
        "root-prompt-cache-key",
        "2026-08-09T00:00:00Z",
    );
    let journal = Journal::create(directory.path(), header.clone()).expect("create journal");
    assert_eq!(journal.state().header(), &header);
    assert_eq!(
        journal.state().active_session_id(),
        header.root_session_id()
    );
    drop(journal);

    let reopened = Journal::open(directory.path(), header.root_session_id()).expect("open journal");
    assert_eq!(reopened.state().header(), &header);
    assert_eq!(
        reopened.state().active_session_id(),
        header.root_session_id()
    );
}

#[test]
fn replay_rejects_a_journal_whose_filename_does_not_match_its_root_header() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let journal = Journal::create(directory.path(), header).expect("create journal");
    drop(journal);
    fs::rename(
        directory.path().join("root-session.jsonl"),
        directory.path().join("other-session.jsonl"),
    )
    .expect("rename journal");

    assert!(matches!(
        Journal::open(directory.path(), "other-session"),
        Err(JournalError::InvalidData(_))
    ));
}

#[test]
fn journal_root_id_must_be_a_single_path_component() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("../outside", "root-cache-key", "2026-08-09T00:00:00Z");
    assert!(matches!(
        Journal::create(directory.path(), header),
        Err(JournalError::InvalidData(_))
    ));
    assert!(!directory.path().join("outside.jsonl").exists());
}

#[test]
fn prepared_commit_and_delivery_round_trip_with_one_active_child() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let mut journal = Journal::create(directory.path(), header.clone()).expect("create journal");
    let intent = TransitionIntent::open("root-session", "call-open", "inspect the parser");
    journal.prepare(intent.clone()).expect("prepare open");
    assert_eq!(journal.state().pending(), Some(&intent));

    journal
        .commit(intent.clone(), "child-session", None, "delivery-open")
        .expect("commit open");
    assert_eq!(journal.state().active_session_id(), "child-session");
    assert_eq!(journal.state().pending(), None);
    assert_eq!(
        journal.state().delivery_status("delivery-open"),
        Some(DeliveryStatus::Unclaimed)
    );

    journal
        .claim_delivery(
            "delivery-open",
            &intent,
            "child-session",
            DeliveryKind::Continuation,
        )
        .expect("claim continuation delivery");
    journal
        .accept_delivery("delivery-open", "child-session")
        .expect("record accepted delivery");
    assert_eq!(
        journal.state().delivery_status("delivery-open"),
        Some(DeliveryStatus::Accepted)
    );
    drop(journal);

    let reopened =
        Journal::open(directory.path(), header.root_session_id()).expect("reopen journal");
    assert_eq!(reopened.state().active_session_id(), "child-session");
    assert_eq!(reopened.state().pending(), None);
    assert_eq!(
        reopened.state().delivery_status("delivery-open"),
        Some(DeliveryStatus::Accepted)
    );
}

#[test]
fn aborted_close_restores_the_active_child_and_records_a_recovery_delivery() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let mut journal = Journal::create(directory.path(), header.clone()).expect("create journal");
    let open = TransitionIntent::open("root-session", "call-open", "inspect the parser");
    journal.prepare(open.clone()).expect("prepare open");
    journal
        .commit(open, "child-session", None, "delivery-open")
        .expect("commit open");

    let close = TransitionIntent::close(
        "child-session",
        "call-close",
        "root-session",
        "the parser has one state machine",
    );
    journal.prepare(close.clone()).expect("prepare close");
    journal
        .abort(
            close.clone(),
            "the terminal turn was cancelled",
            Some("delivery-recover".to_owned()),
        )
        .expect("abort close");
    assert_eq!(journal.state().active_session_id(), "child-session");
    assert_eq!(journal.state().pending(), None);
    assert_eq!(
        journal.state().delivery_status("delivery-recover"),
        Some(DeliveryStatus::Unclaimed)
    );

    journal
        .claim_delivery(
            "delivery-recover",
            &close,
            "child-session",
            DeliveryKind::Recovery,
        )
        .expect("claim recovery delivery");
    journal
        .accept_delivery("delivery-recover", "child-session")
        .expect("record accepted recovery delivery");
    drop(journal);

    let reopened =
        Journal::open(directory.path(), header.root_session_id()).expect("reopen journal");
    assert_eq!(reopened.state().active_session_id(), "child-session");
    assert_eq!(reopened.state().pending(), None);
    assert_eq!(
        reopened.state().delivery_status("delivery-recover"),
        Some(DeliveryStatus::Accepted)
    );
}

#[test]
fn committed_transitions_replay_the_same_reducer_projection() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let mut journal = Journal::create(directory.path(), header.clone()).expect("create journal");
    let open = TransitionIntent::open("root-session", "call-open", "inspect the parser");
    journal.prepare(open.clone()).expect("prepare open");
    journal
        .commit(open, "child-session", None, "delivery-open")
        .expect("commit open");

    let close = TransitionIntent::close(
        "child-session",
        "call-close",
        "root-session",
        "the parser has one state machine",
    );
    journal.prepare(close.clone()).expect("prepare close");
    journal
        .commit(
            close,
            "root-session",
            Some("child-session".to_owned()),
            "delivery-close",
        )
        .expect("commit close");
    let expected = journal.state().projection();
    assert_eq!(expected.nodes.len(), 2);
    assert_eq!(expected.cursor, expected.nodes[0].id);
    assert_eq!(expected.nodes[0].status, NodeStatus::Live);
    assert_eq!(expected.nodes[1].status, NodeStatus::Closed);
    drop(journal);

    let reopened =
        Journal::open(directory.path(), header.root_session_id()).expect("reopen journal");
    assert_eq!(reopened.state().projection(), expected);
}

#[test]
fn committed_next_replays_a_closed_child_and_live_sibling() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let mut journal = Journal::create(directory.path(), header.clone()).expect("create journal");
    let open = TransitionIntent::open("root-session", "call-open", "inspect the parser");
    journal.prepare(open.clone()).expect("prepare open");
    journal
        .commit(open, "child-session", None, "delivery-open")
        .expect("commit open");

    let next = TransitionIntent::next(
        "child-session",
        "call-next",
        "root-session",
        "document the parser invariants",
        "the parser state machine is complete",
    );
    journal.prepare(next.clone()).expect("prepare next");
    journal
        .commit(
            next,
            "sibling-session",
            Some("child-session".to_owned()),
            "delivery-next",
        )
        .expect("commit next");
    let expected = journal.state().projection();
    assert_eq!(journal.state().active_session_id(), "sibling-session");
    assert_eq!(expected.nodes.len(), 3);
    assert_eq!(expected.nodes[0].status, NodeStatus::Opened);
    assert_eq!(expected.nodes[1].status, NodeStatus::Closed);
    assert_eq!(expected.nodes[2].status, NodeStatus::Live);
    drop(journal);

    let reopened =
        Journal::open(directory.path(), header.root_session_id()).expect("reopen journal");
    assert_eq!(reopened.state().projection(), expected);
    assert_eq!(reopened.state().active_session_id(), "sibling-session");
}

#[test]
fn oversized_transition_is_rejected_without_advancing_or_poisoning_the_journal() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let mut journal = Journal::create(directory.path(), header).expect("create journal");
    let oversized = TransitionIntent::open("root-session", "call-open", "x".repeat(64 * 1024));
    assert!(matches!(
        journal.prepare(oversized),
        Err(JournalError::InvalidData(_))
    ));
    assert_eq!(journal.state().pending(), None);

    let valid = TransitionIntent::open("root-session", "call-valid", "inspect the parser");
    journal
        .prepare(valid.clone())
        .expect("accept later valid transition");
    assert_eq!(journal.state().pending(), Some(&valid));
}

#[test]
fn failed_append_poisons_the_writer_and_leaves_no_replayable_transition() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let path = directory.path().join("root-session.jsonl");
    let mut journal = Journal::create(directory.path(), header.clone()).expect("create journal");
    let original = fs::read(&path).expect("read header-only journal");
    journal.fail_next_sync_for_test();

    let intent = TransitionIntent::open("root-session", "call-open", "inspect the parser");
    assert!(matches!(
        journal.prepare(intent),
        Err(JournalError::Io { .. })
    ));
    assert_eq!(journal.state().pending(), None);
    assert!(matches!(
        journal.prepare(TransitionIntent::open(
            "root-session",
            "call-after-failure",
            "must not append",
        )),
        Err(JournalError::Poisoned)
    ));
    assert_eq!(fs::read(&path).expect("read rolled-back journal"), original);
    drop(journal);

    let reopened =
        Journal::open(directory.path(), header.root_session_id()).expect("reopen journal");
    assert_eq!(reopened.state().active_session_id(), "root-session");
    assert_eq!(reopened.state().pending(), None);
}

#[test]
fn rollback_failure_reports_journal_corruption_and_poisons_the_writer() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let mut journal = Journal::create(directory.path(), header).expect("create journal");
    journal.fail_next_sync_for_test();
    journal.fail_next_truncate_for_test();

    let intent = TransitionIntent::open("root-session", "call-open", "inspect the parser");
    let error = journal.prepare(intent).expect_err("append must fail");
    let JournalError::Corrupted { write, rollback } = error else {
        panic!("expected a corruption error");
    };
    assert!(write.to_string().contains("injected journal sync failure"));
    assert!(
        rollback
            .to_string()
            .contains("injected journal truncate failure")
    );
    assert!(matches!(
        journal.prepare(TransitionIntent::open(
            "root-session",
            "call-after-corruption",
            "must not append",
        )),
        Err(JournalError::Poisoned)
    ));
}

#[test]
fn incomplete_final_record_is_truncated_after_replaying_the_valid_prefix() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let path = directory.path().join("root-session.jsonl");
    let mut journal = Journal::create(directory.path(), header.clone()).expect("create journal");
    let intent = TransitionIntent::open("root-session", "call-open", "inspect the parser");
    journal.prepare(intent.clone()).expect("prepare transition");
    drop(journal);

    let valid_prefix = fs::read(&path).expect("read valid journal prefix");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open journal tail");
    file.write_all(br#"{"seq":2,"type":"committed","payload":{"intent":"#)
        .expect("append incomplete tail");
    drop(file);

    let reopened =
        Journal::open(directory.path(), header.root_session_id()).expect("reopen journal");
    assert_eq!(reopened.state().pending(), Some(&intent));
    assert_eq!(
        fs::read(&path).expect("read truncated journal"),
        valid_prefix
    );
}

#[test]
fn complete_final_record_without_a_newline_is_rejected_as_corruption() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let path = directory.path().join("root-session.jsonl");
    let journal = Journal::create(directory.path(), header.clone()).expect("create journal");
    drop(journal);

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open journal tail");
    file.write_all(br#"{"seq":1,"type":"unknown","payload":{}}"#)
        .expect("append complete tail");
    drop(file);

    assert!(matches!(
        Journal::open(directory.path(), header.root_session_id()),
        Err(JournalError::InvalidData(_))
    ));
}

#[test]
fn journal_lock_excludes_a_second_live_coordinator_for_the_same_root() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let journal = Journal::create(directory.path(), header.clone()).expect("create journal");
    assert!(matches!(
        Journal::open(directory.path(), header.root_session_id()),
        Err(JournalError::Locked(_))
    ));
    drop(journal);

    Journal::open(directory.path(), header.root_session_id()).expect("open released journal");
}

#[test]
fn out_of_order_delivery_operations_are_rejected_without_advancing_state() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let mut journal = Journal::create(directory.path(), header).expect("create journal");
    let intent = TransitionIntent::open("root-session", "call-open", "inspect the parser");
    assert!(matches!(
        journal.claim_delivery(
            "delivery-open",
            &intent,
            "child-session",
            DeliveryKind::Continuation,
        ),
        Err(JournalError::InvalidData(_))
    ));
    journal.prepare(intent.clone()).expect("prepare open");
    assert!(matches!(
        journal.commit(intent.clone(), "root-session", None, "delivery-open"),
        Err(JournalError::InvalidData(_))
    ));
    assert_eq!(journal.state().pending(), Some(&intent));

    journal
        .commit(intent.clone(), "child-session", None, "delivery-open")
        .expect("commit open");
    assert!(matches!(
        journal.accept_delivery("delivery-open", "child-session"),
        Err(JournalError::InvalidData(_))
    ));
    journal
        .claim_delivery(
            "delivery-open",
            &intent,
            "child-session",
            DeliveryKind::Continuation,
        )
        .expect("claim delivery");
    assert!(matches!(
        journal.claim_delivery(
            "delivery-open",
            &intent,
            "child-session",
            DeliveryKind::Continuation,
        ),
        Err(JournalError::InvalidData(_))
    ));
    journal
        .accept_delivery("delivery-open", "child-session")
        .expect("accept delivery");
    assert!(matches!(
        journal.accept_delivery("delivery-open", "child-session"),
        Err(JournalError::InvalidData(_))
    ));
    assert_eq!(
        journal.state().delivery_status("delivery-open"),
        Some(DeliveryStatus::Accepted)
    );
}

#[test]
fn next_rejects_a_parent_session_that_is_not_the_active_node_parent() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let mut journal = Journal::create(directory.path(), header).expect("create journal");
    let child = TransitionIntent::open("root-session", "call-child", "inspect the parser");
    journal.prepare(child.clone()).expect("prepare child");
    journal
        .commit(child, "child-session", None, "delivery-child")
        .expect("commit child");
    let grandchild = TransitionIntent::open("child-session", "call-grandchild", "inspect tokens");
    journal
        .prepare(grandchild.clone())
        .expect("prepare grandchild");
    journal
        .commit(
            grandchild,
            "grandchild-session",
            None,
            "delivery-grandchild",
        )
        .expect("commit grandchild");

    let invalid_next = TransitionIntent::next(
        "grandchild-session",
        "call-next",
        "root-session",
        "document the tokens",
        "token work is complete",
    );
    journal
        .prepare(invalid_next.clone())
        .expect("prepare terminal control");
    assert!(matches!(
        journal.commit(
            invalid_next.clone(),
            "sibling-session",
            Some("grandchild-session".to_owned()),
            "delivery-next",
        ),
        Err(JournalError::InvalidData(_))
    ));
    assert_eq!(journal.state().active_session_id(), "grandchild-session");
    assert_eq!(journal.state().pending(), Some(&invalid_next));
}

#[test]
fn committed_child_session_ids_cannot_be_reused_after_they_close() {
    let directory = tempdir().expect("temporary journal directory");
    let header = JournalHeader::new("root-session", "root-cache-key", "2026-08-09T00:00:00Z");
    let mut journal = Journal::create(directory.path(), header).expect("create journal");
    let open = TransitionIntent::open("root-session", "call-open", "inspect the parser");
    journal.prepare(open.clone()).expect("prepare open");
    journal
        .commit(open, "child-session", None, "delivery-open")
        .expect("commit open");
    let close = TransitionIntent::close(
        "child-session",
        "call-close",
        "root-session",
        "the parser is complete",
    );
    journal.prepare(close.clone()).expect("prepare close");
    journal
        .commit(
            close,
            "root-session",
            Some("child-session".to_owned()),
            "delivery-close",
        )
        .expect("commit close");

    let second_open =
        TransitionIntent::open("root-session", "call-second-open", "inspect the output");
    journal
        .prepare(second_open.clone())
        .expect("prepare second open");
    assert!(matches!(
        journal.commit(
            second_open.clone(),
            "child-session",
            None,
            "delivery-second-open"
        ),
        Err(JournalError::InvalidData(_))
    ));
    assert_eq!(journal.state().active_session_id(), "root-session");
    assert_eq!(journal.state().pending(), Some(&second_open));
}

#[test]
fn strict_replay_rejects_unknown_fields_versions_types_and_sequence_gaps() {
    let valid_header = concat!(
        r#"{"seq":0,"type":"header","payload":{"schema_version":1,"root_session_id":"root-session","prompt_cache_key":"root-cache-key","created_at":"2026-08-09T00:00:00Z}}"#,
        "\n"
    );
    let cases = [
        (
            "unknown envelope field",
            format!(
                r#"{{"seq":0,"type":"header","payload":{{"schema_version":1,"root_session_id":"root-session","prompt_cache_key":"root-cache-key","created_at":"2026-08-09T00:00:00Z"}},"extra":true}}"#
            ),
        ),
        (
            "unknown schema version",
            r#"{"seq":0,"type":"header","payload":{"schema_version":2,"root_session_id":"root-session","prompt_cache_key":"root-cache-key","created_at":"2026-08-09T00:00:00Z}}"#.to_owned(),
        ),
        (
            "unknown record type",
            format!(r#"{valid_header}{{"seq":1,"type":"future","payload":{{}}}}"#),
        ),
        (
            "sequence gap",
            format!(r#"{valid_header}{{"seq":2,"type":"future","payload":{{}}}}"#),
        ),
    ];

    for (name, content) in cases {
        let directory = tempdir().expect("temporary journal directory");
        let path = directory.path().join("root-session.jsonl");
        fs::write(&path, format!("{content}\n")).expect("write malformed journal");
        assert!(
            matches!(
                Journal::open(directory.path(), "root-session"),
                Err(JournalError::InvalidData(_))
            ),
            "{name} must be rejected"
        );
    }
}

#[test]
fn replay_rejects_a_record_larger_than_the_hard_line_limit() {
    let directory = tempdir().expect("temporary journal directory");
    let path = directory.path().join("root-session.jsonl");
    let header = concat!(
        r#"{"seq":0,"type":"header","payload":{"schema_version":1,"root_session_id":"root-session","prompt_cache_key":"root-cache-key","created_at":"2026-08-09T00:00:00Z}}"#,
        "\n"
    );
    fs::write(&path, format!("{header}{}\n", "x".repeat(64 * 1024 + 1)))
        .expect("write oversized journal");

    assert!(matches!(
        Journal::open(directory.path(), "root-session"),
        Err(JournalError::InvalidData(_))
    ));
}
