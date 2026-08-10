use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

use codex_spine_core::{
    NodeId, RawBoundary, RolloutEvent, SpineProjection, SpineReducer, ToolCallGroup, ToolOutcome,
    ToolUse,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::SpineAbortReason;

const SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransitionKind {
    Open,
    Close,
    Next,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransitionIntent {
    source_session_id: String,
    terminal_call_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    parent_session_id: Option<String>,
    kind: TransitionKind,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    summary: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    memory: Option<String>,
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl TransitionIntent {
    pub(crate) fn open(
        source_session_id: impl Into<String>,
        terminal_call_id: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            source_session_id: source_session_id.into(),
            terminal_call_id: terminal_call_id.into(),
            parent_session_id: None,
            kind: TransitionKind::Open,
            summary: Some(summary.into()),
            memory: None,
        }
    }

    pub(crate) fn close(
        source_session_id: impl Into<String>,
        terminal_call_id: impl Into<String>,
        parent_session_id: impl Into<String>,
        memory: impl Into<String>,
    ) -> Self {
        Self {
            source_session_id: source_session_id.into(),
            terminal_call_id: terminal_call_id.into(),
            parent_session_id: Some(parent_session_id.into()),
            kind: TransitionKind::Close,
            summary: None,
            memory: Some(memory.into()),
        }
    }

    pub(crate) fn next(
        source_session_id: impl Into<String>,
        terminal_call_id: impl Into<String>,
        parent_session_id: impl Into<String>,
        summary: impl Into<String>,
        memory: impl Into<String>,
    ) -> Self {
        Self {
            source_session_id: source_session_id.into(),
            terminal_call_id: terminal_call_id.into(),
            parent_session_id: Some(parent_session_id.into()),
            kind: TransitionKind::Next,
            summary: Some(summary.into()),
            memory: Some(memory.into()),
        }
    }

    fn key(&self) -> OperationKey {
        OperationKey {
            source_session_id: self.source_session_id.clone(),
            terminal_call_id: self.terminal_call_id.clone(),
        }
    }

    pub(crate) fn source_session_id(&self) -> &str {
        &self.source_session_id
    }

    pub(crate) fn terminal_call_id(&self) -> &str {
        &self.terminal_call_id
    }

    pub(crate) const fn kind(&self) -> TransitionKind {
        self.kind
    }

    pub(crate) fn parent_session_id(&self) -> Option<&str> {
        self.parent_session_id.as_deref()
    }

    pub(crate) fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    pub(crate) fn memory(&self) -> Option<&str> {
        self.memory.as_deref()
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct OperationKey {
    source_session_id: String,
    terminal_call_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryKind {
    Continuation,
    Recovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryStatus {
    Unclaimed,
    Claimed,
    Accepted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalHeader {
    schema_version: u32,
    root_session_id: String,
    prompt_cache_key: String,
    created_at: String,
}

impl JournalHeader {
    pub(crate) fn new(
        root_session_id: impl Into<String>,
        prompt_cache_key: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            root_session_id: root_session_id.into(),
            prompt_cache_key: prompt_cache_key.into(),
            created_at: created_at.into(),
        }
    }

    pub(crate) fn root_session_id(&self) -> &str {
        &self.root_session_id
    }

    pub(crate) fn prompt_cache_key(&self) -> &str {
        &self.prompt_cache_key
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum JournalError {
    #[error("Spine journal lock is already held: {0}")]
    Locked(PathBuf),
    #[error("Spine journal contains invalid data: {0}")]
    InvalidData(String),
    #[error("Spine journal writer is poisoned after a failed append")]
    Poisoned,
    #[error(
        "Spine journal may be corrupted after write failure `{write}` and rollback failure `{rollback}`"
    )]
    Corrupted {
        write: io::Error,
        rollback: io::Error,
    },
    #[error("Spine journal I/O failed while {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

struct JournalWriter {
    file: File,
    #[cfg(test)]
    fail_next_sync: bool,
    #[cfg(test)]
    fail_next_truncate: bool,
}

impl JournalWriter {
    const fn new(file: File) -> Self {
        Self {
            file,
            #[cfg(test)]
            fail_next_sync: false,
            #[cfg(test)]
            fail_next_truncate: false,
        }
    }

    fn append_line(&mut self, path: &Path, line: &[u8]) -> Result<(), JournalError> {
        let start = self
            .file
            .stream_position()
            .map_err(|source| JournalError::Io {
                action: "capturing journal append position",
                path: path.to_path_buf(),
                source,
            })?;
        let write_result = (|| {
            self.file
                .write_all(line)
                .map_err(|source| JournalError::Io {
                    action: "writing journal",
                    path: path.to_path_buf(),
                    source,
                })?;
            self.file
                .write_all(b"\n")
                .map_err(|source| JournalError::Io {
                    action: "writing journal newline",
                    path: path.to_path_buf(),
                    source,
                })?;
            self.file.flush().map_err(|source| JournalError::Io {
                action: "flushing journal",
                path: path.to_path_buf(),
                source,
            })?;
            self.sync_data(path)
        })();
        match write_result {
            Ok(()) => Ok(()),
            Err(write) => match self.truncate_to(path, start) {
                Ok(()) => Err(write),
                Err(rollback) => Err(JournalError::Corrupted {
                    write: journal_io_error(write),
                    rollback: journal_io_error(rollback),
                }),
            },
        }
    }

    fn truncate_to(&mut self, path: &Path, length: u64) -> Result<(), JournalError> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_truncate) {
            return Err(JournalError::Io {
                action: "truncating journal",
                path: path.to_path_buf(),
                source: io::Error::other("injected journal truncate failure"),
            });
        }
        self.file
            .set_len(length)
            .map_err(|source| JournalError::Io {
                action: "truncating journal",
                path: path.to_path_buf(),
                source,
            })?;
        self.file
            .seek(SeekFrom::Start(length))
            .map_err(|source| JournalError::Io {
                action: "seeking truncated journal",
                path: path.to_path_buf(),
                source,
            })?;
        self.file.sync_data().map_err(|source| JournalError::Io {
            action: "syncing truncated journal",
            path: path.to_path_buf(),
            source,
        })
    }

    fn seek_to_end(&mut self, path: &Path) -> Result<(), JournalError> {
        self.file
            .seek(SeekFrom::End(0))
            .map(|_| ())
            .map_err(|source| JournalError::Io {
                action: "seeking journal for append",
                path: path.to_path_buf(),
                source,
            })
    }

    fn sync_data(&mut self, path: &Path) -> Result<(), JournalError> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_sync) {
            return Err(JournalError::Io {
                action: "syncing journal",
                path: path.to_path_buf(),
                source: io::Error::other("injected journal sync failure"),
            });
        }
        self.file.sync_data().map_err(|source| JournalError::Io {
            action: "syncing journal",
            path: path.to_path_buf(),
            source,
        })
    }

    #[cfg(test)]
    const fn fail_next_sync(&mut self) {
        self.fail_next_sync = true;
    }

    #[cfg(test)]
    const fn fail_next_truncate(&mut self) {
        self.fail_next_truncate = true;
    }
}

fn journal_io_error(error: JournalError) -> io::Error {
    match error {
        JournalError::Io { source, .. } => source,
        other => io::Error::other(other.to_string()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JournalState {
    header: JournalHeader,
    active_session_id: String,
    node_sessions: BTreeMap<String, NodeId>,
    pending: Option<TransitionIntent>,
    resolved: BTreeMap<OperationKey, Resolution>,
    deliveries: BTreeMap<String, Delivery>,
    reducer: SpineReducer,
    next_boundary: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Resolution {
    Committed,
    Aborted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Delivery {
    operation: TransitionIntent,
    target_session_id: String,
    kind: DeliveryKind,
    status: DeliveryStatus,
}

impl JournalState {
    pub(crate) const fn header(&self) -> &JournalHeader {
        &self.header
    }

    pub(crate) fn active_session_id(&self) -> &str {
        &self.active_session_id
    }

    pub(crate) fn projection(&self) -> SpineProjection {
        self.reducer.projection()
    }

    pub(crate) const fn pending(&self) -> Option<&TransitionIntent> {
        self.pending.as_ref()
    }

    pub(crate) fn delivery_status(&self, delivery_id: &str) -> Option<DeliveryStatus> {
        self.deliveries
            .get(delivery_id)
            .map(|delivery| delivery.status)
    }

    pub(crate) fn active_delivery_ids(&self) -> BTreeSet<String> {
        self.deliveries
            .iter()
            .filter(|(_, delivery)| delivery.target_session_id == self.active_session_id)
            .map(|(delivery_id, _)| delivery_id.clone())
            .collect()
    }

    pub(crate) fn active_parent_session_id(&self) -> Option<String> {
        let projection = self.reducer.projection();
        let parent = projection
            .nodes
            .iter()
            .find(|node| node.id == projection.cursor)
            .and_then(|node| node.parent.as_ref())?;
        self.node_sessions
            .iter()
            .find_map(|(session_id, node_id)| (node_id == parent).then(|| session_id.clone()))
    }

    pub(crate) fn unclaimed_active_delivery(
        &self,
    ) -> Option<(String, String, TransitionIntent, DeliveryKind)> {
        self.active_delivery(DeliveryStatus::Unclaimed)
    }

    pub(crate) fn claimed_active_delivery(
        &self,
    ) -> Option<(String, String, TransitionIntent, DeliveryKind)> {
        self.active_delivery(DeliveryStatus::Claimed)
    }

    fn active_delivery(
        &self,
        status: DeliveryStatus,
    ) -> Option<(String, String, TransitionIntent, DeliveryKind)> {
        self.deliveries.iter().find_map(|(delivery_id, delivery)| {
            (delivery.status == status && delivery.target_session_id == self.active_session_id)
                .then(|| {
                    (
                        delivery_id.clone(),
                        delivery.target_session_id.clone(),
                        delivery.operation.clone(),
                        delivery.kind,
                    )
                })
        })
    }

    pub(crate) fn task_node_count(&self) -> u32 {
        self.reducer
            .projection()
            .nodes
            .iter()
            .filter(|node| node.kind == codex_spine_core::NodeKind::Task)
            .count()
            .try_into()
            .unwrap_or(u32::MAX)
    }

    fn apply(&mut self, record: &JournalRecord) -> Result<(), JournalError> {
        match record {
            JournalRecord::Prepared(intent) => {
                validate_intent(intent)?;
                if self.pending.is_some() {
                    return Err(JournalError::InvalidData(
                        "journal already has an unresolved prepared transition".to_owned(),
                    ));
                }
                if intent.source_session_id != self.active_session_id {
                    return Err(JournalError::InvalidData(
                        "prepared transition source is not the active session".to_owned(),
                    ));
                }
                if self.resolved.contains_key(&intent.key()) {
                    return Err(JournalError::InvalidData(
                        "journal operation was already resolved".to_owned(),
                    ));
                }
                self.pending = Some(intent.clone());
            }
            JournalRecord::Committed(committed) => {
                validate_intent(&committed.intent)?;
                if self.pending.as_ref() != Some(&committed.intent) {
                    return Err(JournalError::InvalidData(
                        "committed transition does not match the prepared transition".to_owned(),
                    ));
                }
                validate_committed(committed)?;
                if self.deliveries.contains_key(&committed.delivery_id) {
                    return Err(JournalError::InvalidData(
                        "journal delivery ID was already used".to_owned(),
                    ));
                }
                if matches!(
                    committed.intent.kind,
                    TransitionKind::Open | TransitionKind::Next
                ) && self
                    .node_sessions
                    .contains_key(&committed.active_session_id)
                {
                    return Err(JournalError::InvalidData(
                        "committed child session ID was already used".to_owned(),
                    ));
                }
                self.apply_committed_transition(committed)?;
                self.active_session_id = committed.active_session_id.clone();
                self.pending = None;
                self.resolved
                    .insert(committed.intent.key(), Resolution::Committed);
                self.deliveries.insert(
                    committed.delivery_id.clone(),
                    Delivery {
                        operation: committed.intent.clone(),
                        target_session_id: committed.active_session_id.clone(),
                        kind: DeliveryKind::Continuation,
                        status: DeliveryStatus::Unclaimed,
                    },
                );
            }
            JournalRecord::Aborted(aborted) => {
                validate_intent(&aborted.intent)?;
                if self.pending.as_ref() != Some(&aborted.intent) {
                    return Err(JournalError::InvalidData(
                        "aborted transition does not match the prepared transition".to_owned(),
                    ));
                }
                self.pending = None;
                self.resolved
                    .insert(aborted.intent.key(), Resolution::Aborted);
                if let Some(delivery_id) = &aborted.recovery_delivery_id {
                    if delivery_id.trim().is_empty() || self.deliveries.contains_key(delivery_id) {
                        return Err(JournalError::InvalidData(
                            "journal recovery delivery ID must be unique and non-empty".to_owned(),
                        ));
                    }
                    self.deliveries.insert(
                        delivery_id.clone(),
                        Delivery {
                            operation: aborted.intent.clone(),
                            target_session_id: self.active_session_id.clone(),
                            kind: DeliveryKind::Recovery,
                            status: DeliveryStatus::Unclaimed,
                        },
                    );
                }
            }
            JournalRecord::DeliveryClaimed(claimed) => {
                let delivery = self
                    .deliveries
                    .get_mut(&claimed.delivery_id)
                    .ok_or_else(|| {
                        JournalError::InvalidData(
                            "delivery claim has no resolved transition".to_owned(),
                        )
                    })?;
                if delivery.status != DeliveryStatus::Unclaimed
                    || delivery.operation != claimed.intent
                    || delivery.target_session_id != claimed.target_session_id
                    || delivery.kind != claimed.kind
                {
                    return Err(JournalError::InvalidData(
                        "journal delivery claim does not match its pending delivery".to_owned(),
                    ));
                }
                delivery.status = DeliveryStatus::Claimed;
            }
            JournalRecord::DeliveryAccepted(accepted) => {
                let delivery = self
                    .deliveries
                    .get_mut(&accepted.delivery_id)
                    .ok_or_else(|| {
                        JournalError::InvalidData(
                            "delivery acceptance has no claimed transition".to_owned(),
                        )
                    })?;
                if delivery.status != DeliveryStatus::Claimed
                    || delivery.target_session_id != accepted.target_session_id
                {
                    return Err(JournalError::InvalidData(
                        "journal delivery acceptance does not match its claim".to_owned(),
                    ));
                }
                delivery.status = DeliveryStatus::Accepted;
            }
        }
        Ok(())
    }

    fn apply_committed_transition(
        &mut self,
        committed: &CommittedRecord,
    ) -> Result<(), JournalError> {
        let intent = &committed.intent;
        let projection = self.reducer.projection();
        let current = projection.cursor.clone();
        if self.node_sessions.get(&intent.source_session_id) != Some(&current) {
            return Err(JournalError::InvalidData(
                "committed transition source does not own the reducer cursor".to_owned(),
            ));
        }
        match intent.kind {
            TransitionKind::Open => {
                apply_reducer_control(
                    &mut self.reducer,
                    &mut self.next_boundary,
                    intent,
                    "spine.open",
                    json!({ "summary": intent.summary.as_deref() }),
                )?;
                self.node_sessions.insert(
                    committed.active_session_id.clone(),
                    self.reducer.projection().cursor,
                );
            }
            TransitionKind::Close => {
                let parent_session_id = intent.parent_session_id.as_deref().ok_or_else(|| {
                    JournalError::InvalidData("close transition has no parent session".to_owned())
                })?;
                let parent = self
                    .node_sessions
                    .get(parent_session_id)
                    .ok_or_else(|| {
                        JournalError::InvalidData(
                            "close transition parent session is unknown".to_owned(),
                        )
                    })?
                    .clone();
                let reducer_parent = projection
                    .nodes
                    .iter()
                    .find(|node| node.id == current)
                    .and_then(|node| node.parent.clone())
                    .ok_or_else(|| {
                        JournalError::InvalidData(
                            "close transition source has no reducer parent".to_owned(),
                        )
                    })?;
                if parent != reducer_parent {
                    return Err(JournalError::InvalidData(
                        "close transition parent does not own the reducer parent node".to_owned(),
                    ));
                }
                apply_reducer_control(
                    &mut self.reducer,
                    &mut self.next_boundary,
                    intent,
                    "spine.close",
                    json!({ "memory": intent.memory.as_deref() }),
                )?;
                if self.reducer.projection().cursor != parent {
                    return Err(JournalError::InvalidData(
                        "committed close did not restore its parent reducer node".to_owned(),
                    ));
                }
            }
            TransitionKind::Next => {
                let parent_session_id = intent.parent_session_id.as_deref().ok_or_else(|| {
                    JournalError::InvalidData("next transition has no parent session".to_owned())
                })?;
                let Some(parent) = self.node_sessions.get(parent_session_id) else {
                    return Err(JournalError::InvalidData(
                        "next transition parent session is unknown".to_owned(),
                    ));
                };
                let reducer_parent = projection
                    .nodes
                    .iter()
                    .find(|node| node.id == current)
                    .and_then(|node| node.parent.clone())
                    .ok_or_else(|| {
                        JournalError::InvalidData(
                            "next transition source has no reducer parent".to_owned(),
                        )
                    })?;
                if *parent != reducer_parent {
                    return Err(JournalError::InvalidData(
                        "next transition parent does not own the reducer parent node".to_owned(),
                    ));
                }
                apply_reducer_control(
                    &mut self.reducer,
                    &mut self.next_boundary,
                    intent,
                    "spine.next",
                    json!({
                        "summary": intent.summary.as_deref(),
                        "memory": intent.memory.as_deref(),
                    }),
                )?;
                self.node_sessions.insert(
                    committed.active_session_id.clone(),
                    self.reducer.projection().cursor,
                );
            }
        }
        Ok(())
    }
}

pub(crate) struct Journal {
    _lock: File,
    writer: JournalWriter,
    path: PathBuf,
    state: JournalState,
    next_seq: u64,
    poisoned: bool,
}

impl Journal {
    pub(crate) fn create(directory: &Path, header: JournalHeader) -> Result<Self, JournalError> {
        validate_header(&header)?;
        fs::create_dir_all(directory).map_err(|source| JournalError::Io {
            action: "creating journal directory",
            path: directory.to_path_buf(),
            source,
        })?;
        let path = journal_path(directory, header.root_session_id());
        let lock = acquire_lock(directory, header.root_session_id())?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| JournalError::Io {
                action: "creating journal",
                path: path.clone(),
                source,
            })?;
        let mut writer = JournalWriter::new(file);
        write_header(&mut writer, &path, &header)?;
        Ok(Self {
            _lock: lock,
            writer,
            path,
            state: JournalState {
                active_session_id: header.root_session_id.clone(),
                node_sessions: BTreeMap::from([(
                    header.root_session_id.clone(),
                    SpineReducer::new().projection().cursor,
                )]),
                header,
                pending: None,
                resolved: BTreeMap::new(),
                deliveries: BTreeMap::new(),
                reducer: SpineReducer::new(),
                next_boundary: 1,
            },
            next_seq: 1,
            poisoned: false,
        })
    }

    pub(crate) fn open(directory: &Path, root_session_id: &str) -> Result<Self, JournalError> {
        validate_root_session_id(root_session_id)?;
        let path = journal_path(directory, root_session_id);
        let lock = acquire_lock(directory, root_session_id)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| JournalError::Io {
                action: "opening journal",
                path: path.clone(),
                source,
            })?;
        let mut writer = JournalWriter::new(file);
        let mut bytes = Vec::new();
        writer
            .file
            .read_to_end(&mut bytes)
            .map_err(|source| JournalError::Io {
                action: "reading journal",
                path: path.clone(),
                source,
            })?;
        let replayed = replay(&path, &bytes)?;
        if let Some(length) = replayed.truncate_to {
            writer.truncate_to(&path, length)?;
        } else {
            writer.seek_to_end(&path)?;
        }
        let state = replayed.state;
        let next_seq = replayed.next_seq;
        if state.header.root_session_id() != root_session_id {
            return Err(JournalError::InvalidData(format!(
                "journal filename root `{root_session_id}` does not match header root `{}`",
                state.header.root_session_id()
            )));
        }
        Ok(Self {
            _lock: lock,
            writer,
            path,
            state,
            next_seq,
            poisoned: false,
        })
    }

    pub(crate) const fn state(&self) -> &JournalState {
        &self.state
    }

    pub(crate) fn prepare(&mut self, intent: TransitionIntent) -> Result<(), JournalError> {
        self.append(JournalRecord::Prepared(intent))
    }

    pub(crate) fn commit(
        &mut self,
        intent: TransitionIntent,
        active_session_id: impl Into<String>,
        closed_session_id: Option<String>,
        delivery_id: impl Into<String>,
    ) -> Result<(), JournalError> {
        self.append(JournalRecord::Committed(CommittedRecord {
            intent,
            active_session_id: active_session_id.into(),
            closed_session_id,
            delivery_id: delivery_id.into(),
        }))
    }

    pub(crate) fn abort(
        &mut self,
        intent: TransitionIntent,
        reason: SpineAbortReason,
        recovery_delivery_id: Option<String>,
    ) -> Result<(), JournalError> {
        self.append(JournalRecord::Aborted(AbortedRecord {
            intent,
            reason,
            recovery_delivery_id,
        }))
    }

    pub(crate) fn claim_delivery(
        &mut self,
        delivery_id: impl Into<String>,
        intent: &TransitionIntent,
        target_session_id: impl Into<String>,
        kind: DeliveryKind,
    ) -> Result<(), JournalError> {
        self.append(JournalRecord::DeliveryClaimed(DeliveryClaimedRecord {
            delivery_id: delivery_id.into(),
            intent: intent.clone(),
            target_session_id: target_session_id.into(),
            kind,
        }))
    }

    pub(crate) fn accept_delivery(
        &mut self,
        delivery_id: impl Into<String>,
        target_session_id: impl Into<String>,
    ) -> Result<(), JournalError> {
        self.append(JournalRecord::DeliveryAccepted(DeliveryAcceptedRecord {
            delivery_id: delivery_id.into(),
            target_session_id: target_session_id.into(),
        }))
    }

    fn append(&mut self, record: JournalRecord) -> Result<(), JournalError> {
        if self.poisoned {
            return Err(JournalError::Poisoned);
        }
        let mut candidate = self.state.clone();
        candidate.apply(&record)?;
        let encoded = encode_record(self.next_seq, &record)?;
        if let Err(error) = self.writer.append_line(&self.path, &encoded) {
            self.poisoned = true;
            return Err(error);
        }
        self.state = candidate;
        self.next_seq = self.next_seq.saturating_add(1);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) const fn fail_next_sync_for_test(&mut self) {
        self.writer.fail_next_sync();
    }

    #[cfg(test)]
    pub(crate) const fn fail_next_truncate_for_test(&mut self) {
        self.writer.fail_next_truncate();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum JournalRecord {
    Prepared(TransitionIntent),
    Committed(CommittedRecord),
    Aborted(AbortedRecord),
    DeliveryClaimed(DeliveryClaimedRecord),
    DeliveryAccepted(DeliveryAcceptedRecord),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommittedRecord {
    intent: TransitionIntent,
    active_session_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    closed_session_id: Option<String>,
    delivery_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AbortedRecord {
    intent: TransitionIntent,
    reason: SpineAbortReason,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    recovery_delivery_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryClaimedRecord {
    delivery_id: String,
    intent: TransitionIntent,
    target_session_id: String,
    kind: DeliveryKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryAcceptedRecord {
    delivery_id: String,
    target_session_id: String,
}

fn journal_path(directory: &Path, root_session_id: &str) -> PathBuf {
    directory.join(format!("{root_session_id}.jsonl"))
}

fn acquire_lock(directory: &Path, root_session_id: &str) -> Result<File, JournalError> {
    let path = directory.join(format!("{root_session_id}.lock"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| JournalError::Io {
            action: "opening journal lock",
            path: path.clone(),
            source,
        })?;
    file.try_lock_exclusive().map_err(|source| {
        if source.kind() == io::ErrorKind::WouldBlock {
            JournalError::Locked(path)
        } else {
            JournalError::Io {
                action: "locking journal",
                path,
                source,
            }
        }
    })?;
    Ok(file)
}

fn validate_header(header: &JournalHeader) -> Result<(), JournalError> {
    if header.schema_version != SCHEMA_VERSION {
        return Err(JournalError::InvalidData(format!(
            "unsupported journal schema version {}",
            header.schema_version
        )));
    }
    for (name, value) in [
        ("root session ID", header.root_session_id.as_str()),
        ("prompt cache key", header.prompt_cache_key.as_str()),
        ("created-at timestamp", header.created_at.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(JournalError::InvalidData(format!(
                "journal {name} must not be empty"
            )));
        }
    }
    validate_root_session_id(&header.root_session_id)?;
    Ok(())
}

fn validate_root_session_id(root_session_id: &str) -> Result<(), JournalError> {
    let mut components = Path::new(root_session_id).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(JournalError::InvalidData(
            "journal root session ID must be a single path component".to_owned(),
        ));
    }
    Ok(())
}

fn write_header(
    writer: &mut JournalWriter,
    path: &Path,
    header: &JournalHeader,
) -> Result<(), JournalError> {
    writer.append_line(path, &encode_payload(0, "header", header)?)
}

fn encode_record(seq: u64, record: &JournalRecord) -> Result<Vec<u8>, JournalError> {
    match record {
        JournalRecord::Prepared(payload) => encode_payload(seq, "prepared", payload),
        JournalRecord::Committed(payload) => encode_payload(seq, "committed", payload),
        JournalRecord::Aborted(payload) => encode_payload(seq, "aborted", payload),
        JournalRecord::DeliveryClaimed(payload) => encode_payload(seq, "delivery_claimed", payload),
        JournalRecord::DeliveryAccepted(payload) => {
            encode_payload(seq, "delivery_accepted", payload)
        }
    }
}

fn encode_payload<T: Serialize>(
    seq: u64,
    record_type: &str,
    payload: &T,
) -> Result<Vec<u8>, JournalError> {
    let line = serde_json::to_vec(&serde_json::json!({
        "seq": seq,
        "type": record_type,
        "payload": payload,
    }))
    .map_err(|error| JournalError::InvalidData(format!("encoding journal record: {error}")))?;
    if line.len() > MAX_RECORD_BYTES {
        return Err(JournalError::InvalidData(format!(
            "journal record exceeds the {MAX_RECORD_BYTES}-byte limit"
        )));
    }
    Ok(line)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvelope {
    seq: u64,
    #[serde(rename = "type")]
    record_type: String,
    payload: serde_json::Value,
}

struct ReplayedJournal {
    state: JournalState,
    next_seq: u64,
    truncate_to: Option<u64>,
}

fn replay(path: &Path, bytes: &[u8]) -> Result<ReplayedJournal, JournalError> {
    if bytes.is_empty() {
        return Err(JournalError::InvalidData(
            "journal is missing its header".to_owned(),
        ));
    }
    let (complete, truncate_to) = complete_prefix(path, bytes)?;

    let mut header = None;
    let mut state = None;
    let mut expected_seq = 0;
    for (index, line) in complete[..complete.len().saturating_sub(1)]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if line.len() > MAX_RECORD_BYTES {
            return Err(JournalError::InvalidData(format!(
                "journal {} line {} exceeds the {MAX_RECORD_BYTES}-byte limit",
                path.display(),
                index + 1
            )));
        }
        let envelope = serde_json::from_slice::<RawEnvelope>(line).map_err(|error| {
            JournalError::InvalidData(format!(
                "journal {} line {} is invalid: {error}",
                path.display(),
                index + 1
            ))
        })?;
        if envelope.seq != expected_seq {
            return Err(JournalError::InvalidData(format!(
                "journal sequence gap at line {}: expected {expected_seq}, found {}",
                index + 1,
                envelope.seq
            )));
        }
        if expected_seq == 0 {
            if envelope.record_type != "header" {
                return Err(JournalError::InvalidData(
                    "journal first record must be header sequence 0".to_owned(),
                ));
            }
            let parsed =
                serde_json::from_value::<JournalHeader>(envelope.payload).map_err(|error| {
                    JournalError::InvalidData(format!("journal header payload is invalid: {error}"))
                })?;
            validate_header(&parsed)?;
            state = Some(JournalState {
                active_session_id: parsed.root_session_id.clone(),
                node_sessions: BTreeMap::from([(
                    parsed.root_session_id.clone(),
                    SpineReducer::new().projection().cursor,
                )]),
                header: parsed.clone(),
                pending: None,
                resolved: BTreeMap::new(),
                deliveries: BTreeMap::new(),
                reducer: SpineReducer::new(),
                next_boundary: 1,
            });
            header = Some(parsed);
        } else {
            let record = decode_record(&envelope.record_type, envelope.payload)?;
            let Some(state) = state.as_mut() else {
                return Err(JournalError::InvalidData(
                    "journal record appears before its header".to_owned(),
                ));
            };
            state.apply(&record)?;
        }
        expected_seq = expected_seq.saturating_add(1);
    }
    if header.is_none() {
        return Err(JournalError::InvalidData(
            "journal is missing its header".to_owned(),
        ));
    }
    let Some(state) = state else {
        return Err(JournalError::InvalidData(
            "journal is missing its header".to_owned(),
        ));
    };
    Ok(ReplayedJournal {
        state,
        next_seq: expected_seq,
        truncate_to,
    })
}

fn complete_prefix<'a>(
    path: &Path,
    bytes: &'a [u8],
) -> Result<(&'a [u8], Option<u64>), JournalError> {
    if bytes.ends_with(b"\n") {
        return Ok((bytes, None));
    }
    let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Err(JournalError::InvalidData(format!(
            "journal {} has no complete header record",
            path.display()
        )));
    };
    let complete = &bytes[..=last_newline];
    let tail = &bytes[last_newline + 1..];
    if tail.len() > MAX_RECORD_BYTES {
        return Err(JournalError::InvalidData(format!(
            "journal {} final record exceeds the {MAX_RECORD_BYTES}-byte limit",
            path.display()
        )));
    }
    match serde_json::from_slice::<RawEnvelope>(tail) {
        Err(error) if error.is_eof() => Ok((complete, Some(complete.len() as u64))),
        Ok(_) => Err(JournalError::InvalidData(format!(
            "journal {} final record is complete but missing its newline terminator",
            path.display()
        ))),
        Err(error) => Err(JournalError::InvalidData(format!(
            "journal {} final record is corrupted: {error}",
            path.display()
        ))),
    }
}

fn decode_record(
    record_type: &str,
    payload: serde_json::Value,
) -> Result<JournalRecord, JournalError> {
    let decode = |error: serde_json::Error| {
        JournalError::InvalidData(format!("journal {record_type} payload is invalid: {error}"))
    };
    match record_type {
        "prepared" => serde_json::from_value(payload)
            .map(JournalRecord::Prepared)
            .map_err(decode),
        "committed" => serde_json::from_value(payload)
            .map(JournalRecord::Committed)
            .map_err(decode),
        "aborted" => serde_json::from_value(payload)
            .map(JournalRecord::Aborted)
            .map_err(decode),
        "delivery_claimed" => serde_json::from_value(payload)
            .map(JournalRecord::DeliveryClaimed)
            .map_err(decode),
        "delivery_accepted" => serde_json::from_value(payload)
            .map(JournalRecord::DeliveryAccepted)
            .map_err(decode),
        _ => Err(JournalError::InvalidData(format!(
            "journal record has unknown type `{record_type}`"
        ))),
    }
}

fn validate_intent(intent: &TransitionIntent) -> Result<(), JournalError> {
    for (name, value) in [
        ("source session ID", intent.source_session_id.as_str()),
        ("terminal call ID", intent.terminal_call_id.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(JournalError::InvalidData(format!(
                "transition {name} must not be empty"
            )));
        }
    }
    let required = |name: &str, value: Option<&str>| {
        value
            .filter(|value| !value.trim().is_empty())
            .map(|_| ())
            .ok_or_else(|| JournalError::InvalidData(format!("transition {name} is required")))
    };
    match intent.kind {
        TransitionKind::Open => {
            if intent.parent_session_id.is_some() || intent.memory.is_some() {
                return Err(JournalError::InvalidData(
                    "open transition has forbidden parent or memory".to_owned(),
                ));
            }
            required("summary", intent.summary.as_deref())?;
        }
        TransitionKind::Close => {
            required("parent session ID", intent.parent_session_id.as_deref())?;
            required("memory", intent.memory.as_deref())?;
            if intent.summary.is_some() {
                return Err(JournalError::InvalidData(
                    "close transition has forbidden summary".to_owned(),
                ));
            }
        }
        TransitionKind::Next => {
            required("parent session ID", intent.parent_session_id.as_deref())?;
            required("summary", intent.summary.as_deref())?;
            required("memory", intent.memory.as_deref())?;
        }
    }
    Ok(())
}

fn validate_committed(committed: &CommittedRecord) -> Result<(), JournalError> {
    if committed.active_session_id.trim().is_empty() || committed.delivery_id.trim().is_empty() {
        return Err(JournalError::InvalidData(
            "committed transition active session and delivery IDs must not be empty".to_owned(),
        ));
    }
    let intent = &committed.intent;
    match intent.kind {
        TransitionKind::Open => {
            if committed.closed_session_id.is_some()
                || committed.active_session_id == intent.source_session_id
            {
                return Err(JournalError::InvalidData(
                    "committed open must activate a distinct child without closing a session"
                        .to_owned(),
                ));
            }
        }
        TransitionKind::Close => {
            if committed.closed_session_id.as_deref() != Some(&intent.source_session_id)
                || Some(committed.active_session_id.as_str()) != intent.parent_session_id.as_deref()
            {
                return Err(JournalError::InvalidData(
                    "committed close must activate its parent and close its source".to_owned(),
                ));
            }
        }
        TransitionKind::Next => {
            if committed.closed_session_id.as_deref() != Some(&intent.source_session_id)
                || Some(committed.active_session_id.as_str()) == intent.parent_session_id.as_deref()
                || committed.active_session_id == intent.source_session_id
            {
                return Err(JournalError::InvalidData(
                    "committed next must activate a distinct sibling and close its source"
                        .to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn apply_reducer_control(
    reducer: &mut SpineReducer,
    next_boundary: &mut u64,
    intent: &TransitionIntent,
    name: &str,
    arguments: serde_json::Value,
) -> Result<(), JournalError> {
    let arguments = serde_json::to_string(&arguments).map_err(|error| {
        JournalError::InvalidData(format!("encoding reducer control arguments: {error}"))
    })?;
    let start = RawBoundary(*next_boundary);
    *next_boundary = next_boundary.saturating_add(1);
    let end = RawBoundary(*next_boundary);
    *next_boundary = next_boundary.saturating_add(1);
    reducer.apply(RolloutEvent::ToolCall(ToolCallGroup {
        start,
        end,
        leading_assistant_messages: Vec::new(),
        calls: vec![ToolUse {
            call_id: format!("{}:{}", intent.source_session_id, intent.terminal_call_id),
            name: name.to_owned(),
            arguments,
            call_ordinal: None,
            outcome: Some(ToolOutcome::Succeeded),
            output: Some(r#"{\"accepted\":true}"#.to_owned()),
            output_boundary: Some(end),
        }],
    }));
    Ok(())
}
