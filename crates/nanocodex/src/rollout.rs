use std::{
    fs::File,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use chrono::{Local, SecondsFormat, Utc};
use nanocodex_core::{
    ImageDetail, Prompt, PromptInput, ResponseItem, UserInput, responses::ResponseHistory,
};
use serde::Serialize;
use tokio::{
    io::{AsyncSeekExt, AsyncWriteExt},
    runtime::Handle,
    sync::{mpsc, oneshot},
};
use tracing::error;

use crate::session::{CommittedSession, SessionSnapshot};

const COMMAND_CAPACITY: usize = 8;

/// Configuration for writing a thread in Codex's resumable rollout layout.
#[derive(Clone, Debug)]
pub struct RolloutConfig {
    codex_home: PathBuf,
    resume_path: Option<PathBuf>,
}

impl RolloutConfig {
    /// Writes rollouts beneath `<codex_home>/sessions/YYYY/MM/DD`.
    #[must_use]
    pub fn new(codex_home: impl Into<PathBuf>) -> Self {
        Self {
            codex_home: codex_home.into(),
            resume_path: None,
        }
    }

    /// Returns the Codex state directory used for this rollout policy.
    #[must_use]
    pub fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    /// Loads a Codex or Nanocodex session recorded beneath this Codex home.
    ///
    /// # Errors
    ///
    /// Returns an error when the thread ID is not a UUID, the session does not
    /// exist, or its rollout is malformed or incompatible.
    pub fn load_session(&self, thread_id: &str) -> io::Result<DurableSession> {
        DurableSession::load(&self.codex_home, thread_id)
    }

    pub(crate) fn resumed(mut self, rollout_path: PathBuf) -> Self {
        self.resume_path = Some(rollout_path);
        self
    }
}

/// A completed model boundary materialized from a Codex-compatible rollout.
#[derive(Clone, Debug)]
pub struct DurableSession {
    codex_home: PathBuf,
    thread_id: String,
    rollout_path: PathBuf,
    snapshot: SessionSnapshot,
    transcript: Vec<RolloutTranscriptItem>,
}

impl DurableSession {
    fn load(codex_home: &Path, thread_id: &str) -> io::Result<Self> {
        uuid::Uuid::parse_str(thread_id).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid Codex thread ID `{thread_id}`: {error}"),
            )
        })?;
        let rollout_path = find_rollout_path(codex_home, thread_id)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no Codex rollout found for thread {thread_id}"),
            )
        })?;
        let (workspace, history, transcript) = materialize_rollout(&rollout_path, thread_id)?;
        let snapshot = SessionSnapshot::from_rollout(thread_id.to_owned(), workspace, history)
            .map_err(io::Error::other)?;
        Ok(Self {
            codex_home: codex_home.to_path_buf(),
            thread_id: thread_id.to_owned(),
            rollout_path,
            snapshot,
            transcript,
        })
    }

    /// Returns the stable thread UUID retained across process restarts.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Returns the original workspace restored by this session.
    #[must_use]
    pub fn workspace(&self) -> &str {
        self.snapshot.workspace()
    }

    /// Returns the restored model boundary.
    #[must_use]
    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    /// Returns the Codex-compatible rollout reopened by this session.
    #[must_use]
    pub fn rollout_path(&self) -> &Path {
        &self.rollout_path
    }

    /// Returns the visible activity used to restore the originating transcript.
    #[must_use]
    pub fn transcript(&self) -> &[RolloutTranscriptItem] {
        &self.transcript
    }

    /// Splits this loaded boundary into the builder inputs needed to continue it.
    #[must_use]
    pub fn into_parts(self) -> (String, SessionSnapshot, RolloutConfig) {
        (
            self.thread_id,
            self.snapshot,
            RolloutConfig::new(self.codex_home).resumed(self.rollout_path),
        )
    }
}

/// User-visible activity reconstructed from a Codex-compatible rollout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RolloutTranscriptItem {
    /// A submitted user prompt.
    User(String),
    /// A reasoning summary displayed while the assistant was working.
    Reasoning(String),
    /// An assistant message displayed by the originating client.
    Assistant(String),
    /// A tool invocation displayed by the originating client.
    Tool {
        /// Stable call identifier from the rollout.
        call_id: String,
        /// Tool name sent by the model.
        name: String,
        /// Serialized tool arguments sent by the model.
        arguments: String,
    },
}

fn find_rollout_path(codex_home: &Path, thread_id: &str) -> io::Result<Option<PathBuf>> {
    let suffix = format!("-{thread_id}.jsonl");
    let compressed_suffix = format!("-{thread_id}.jsonl.zst");
    for root in [
        codex_home.join("sessions"),
        codex_home.join("archived_sessions"),
    ] {
        let mut directories = vec![root];
        while let Some(directory) = directories.pop() {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for entry in entries {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    directories.push(entry.path());
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let file_name = entry.file_name();
                let Some(file_name) = file_name.to_str() else {
                    continue;
                };
                if file_name.ends_with(&suffix) {
                    return entry.path().canonicalize().map(Some);
                }
                if file_name.ends_with(&compressed_suffix) {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!(
                            "compressed Codex rollout {} is not supported yet",
                            entry.path().display()
                        ),
                    ));
                }
            }
        }
    }
    Ok(None)
}

fn materialize_rollout(
    path: &Path,
    thread_id: &str,
) -> io::Result<(String, Vec<ResponseItem>, Vec<RolloutTranscriptItem>)> {
    let mut workspace = None;
    let mut history = Vec::new();
    let mut transcript = Vec::new();
    for (index, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        let value: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "failed to decode {} line {}: {error}",
                    path.display(),
                    index + 1
                ),
            )
        })?;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("session_meta") if workspace.is_none() => {
                let payload = &value["payload"];
                if payload.get("id").and_then(serde_json::Value::as_str) != Some(thread_id) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Codex rollout thread ID does not match its filename",
                    ));
                }
                workspace = Some(
                    payload
                        .get("cwd")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Codex rollout session metadata is missing its workspace",
                            )
                        })?
                        .to_owned(),
                );
            }
            Some("response_item") => {
                if let Some(item) = visible_tool_call(&value["payload"]) {
                    transcript.push(item);
                }
                let item = serde_json::from_value(value["payload"].clone()).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "failed to decode response item at {} line {}: {error}",
                            path.display(),
                            index + 1
                        ),
                    )
                })?;
                history.push(item);
            }
            Some("compacted") => {
                history = serde_json::from_value(value["payload"]["replacement_history"].clone())
                    .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "failed to decode replacement history at {} line {}: {error}",
                            path.display(),
                            index + 1
                        ),
                    )
                })?;
            }
            Some("event_msg") => {
                if let Some(item) = visible_rollout_event(&value["payload"]) {
                    transcript.push(item);
                }
            }
            _ => {}
        }
    }
    let workspace = workspace.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex rollout is missing session metadata",
        )
    })?;
    let workspace = Path::new(&workspace).canonicalize()?;
    let workspace = workspace.into_os_string().into_string().map_err(|path| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Codex rollout workspace is not valid UTF-8: {}",
                Path::new(&path).display()
            ),
        )
    })?;
    Ok((workspace, history, transcript))
}

fn visible_rollout_event(payload: &serde_json::Value) -> Option<RolloutTranscriptItem> {
    match payload.get("type")?.as_str()? {
        "user_message" => visible_text(payload, "message").map(RolloutTranscriptItem::User),
        "agent_reasoning" => visible_text(payload, "text").map(RolloutTranscriptItem::Reasoning),
        "agent_message" => visible_text(payload, "message").map(RolloutTranscriptItem::Assistant),
        "mcp_tool_call_end" => {
            let invocation = payload.get("invocation")?;
            let server = invocation.get("server")?.as_str()?;
            let tool = invocation.get("tool")?.as_str()?;
            Some(RolloutTranscriptItem::Tool {
                call_id: payload.get("call_id")?.as_str()?.to_owned(),
                name: format!("{server}.{tool}"),
                arguments: serde_json::to_string(invocation.get("arguments")?).ok()?,
            })
        }
        "web_search_end" => Some(RolloutTranscriptItem::Tool {
            call_id: payload.get("call_id")?.as_str()?.to_owned(),
            name: "web_search".to_owned(),
            arguments: serde_json::to_string(payload.get("action")?).ok()?,
        }),
        _ => None,
    }
}

fn visible_text(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload
        .get(key)?
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn visible_tool_call(payload: &serde_json::Value) -> Option<RolloutTranscriptItem> {
    let (name, arguments) = match payload.get("type")?.as_str()? {
        "custom_tool_call" => (
            payload.get("name")?.as_str()?.to_owned(),
            payload.get("input")?.as_str()?.to_owned(),
        ),
        "function_call" => (
            payload.get("name")?.as_str()?.to_owned(),
            payload.get("arguments")?.as_str()?.to_owned(),
        ),
        _ => return None,
    };
    Some(RolloutTranscriptItem::Tool {
        call_id: payload.get("call_id")?.as_str()?.to_owned(),
        name,
        arguments,
    })
}

/// Stable identity and file location of a recorded Nanocodex thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RolloutInfo {
    thread_id: String,
    path: PathBuf,
}

impl RolloutInfo {
    /// UUID accepted by `codex resume` and `codex exec resume`.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Codex-compatible JSONL rollout path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone)]
pub(crate) struct RolloutRecorder {
    info: RolloutInfo,
    commands: mpsc::Sender<RolloutCommand>,
}

#[derive(Clone, Copy)]
pub(crate) struct RolloutOrigin<'a> {
    pub(crate) kind: &'a str,
    pub(crate) parent_thread_id: Option<&'a str>,
}

enum RolloutCommand {
    Commit {
        commit: Box<RolloutCommit>,
        result: oneshot::Sender<io::Result<()>>,
    },
    Flush {
        result: oneshot::Sender<io::Result<()>>,
    },
}

struct RolloutCommit {
    history: ResponseHistory,
    revision: u64,
    turn: RolloutTurn,
}

impl RolloutCommit {
    fn from_session(session: &CommittedSession, turn: RolloutTurn) -> Self {
        Self {
            history: session.rollout_history(),
            revision: session.history_revision(),
            turn,
        }
    }

    #[cfg(test)]
    fn from_history(history: ResponseHistory, revision: u64, turn: RolloutTurn) -> Self {
        Self {
            history,
            revision,
            turn,
        }
    }
}

#[derive(Clone)]
pub(crate) struct RolloutTurn {
    turn_id: String,
    user_message: UserMessage,
    final_message: Option<String>,
    started_at: i64,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    status: RolloutTurnStatus,
    timer: Option<Instant>,
}

#[derive(Clone, Copy)]
enum RolloutTurnStatus {
    InProgress,
    Completed,
    Interrupted,
}

impl RolloutTurn {
    pub(crate) fn started(prompt: &Prompt) -> Self {
        Self {
            turn_id: uuid::Uuid::now_v7().to_string(),
            user_message: UserMessage::from_prompt(prompt),
            final_message: None,
            started_at: Utc::now().timestamp(),
            completed_at: None,
            duration_ms: None,
            status: RolloutTurnStatus::InProgress,
            timer: Some(Instant::now()),
        }
    }

    pub(crate) fn completed(mut self, final_message: String) -> Self {
        self.finish(RolloutTurnStatus::Completed);
        self.final_message = Some(final_message);
        self
    }

    pub(crate) fn interrupted(mut self) -> Self {
        self.finish(RolloutTurnStatus::Interrupted);
        self
    }

    fn finish(&mut self, status: RolloutTurnStatus) {
        self.status = status;
        self.completed_at = Some(Utc::now().timestamp());
        self.duration_ms = self
            .timer
            .take()
            .map(|started| i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
    }
}

impl RolloutRecorder {
    pub(crate) fn create(
        runtime: &Handle,
        config: &RolloutConfig,
        thread_id: &str,
        cwd: &Path,
        instructions: &str,
        origin: RolloutOrigin<'_>,
        resume_history_len: Option<usize>,
    ) -> io::Result<Self> {
        if let Some(path) = &config.resume_path {
            let history_len = resume_history_len.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "resumed rollout requires restored history",
                )
            })?;
            return Self::resume(runtime, thread_id, path, history_len);
        }
        let local = Local::now();
        let directory = config
            .codex_home
            .join("sessions")
            .join(local.format("%Y").to_string())
            .join(local.format("%m").to_string())
            .join(local.format("%d").to_string());
        std::fs::create_dir_all(&directory)?;
        let filename_timestamp = local.format("%Y-%m-%dT%H-%M-%S");
        let path = directory.join(format!("rollout-{filename_timestamp}-{thread_id}.jsonl"));
        let initial_window_id = uuid::Uuid::now_v7().to_string();
        let timestamp = timestamp();
        let parent_thread_id = origin.parent_thread_id.map(ToOwned::to_owned);
        let meta = SessionMeta {
            session_id: thread_id.to_owned(),
            id: thread_id.to_owned(),
            forked_from_id: (origin.kind == "fork")
                .then(|| parent_thread_id.clone())
                .flatten(),
            parent_thread_id,
            timestamp: timestamp.clone(),
            cwd: cwd.to_path_buf(),
            originator: "nanocodex".to_owned(),
            cli_version: env!("CARGO_PKG_VERSION").to_owned(),
            source: "cli",
            thread_source: "user",
            model_provider: "openai",
            base_instructions: BaseInstructions {
                text: instructions.to_owned(),
            },
            history_mode: "legacy",
            context_window: SessionContextWindow {
                window_id: initial_window_id.clone(),
            },
        };
        let mut file = File::options().write(true).create_new(true).open(&path)?;
        write_line(
            &mut file,
            &RolloutLine {
                timestamp,
                item: RolloutItem::SessionMeta(&meta),
            },
        )?;
        file.flush()?;
        file.sync_all()?;

        let writer = RolloutWriter::new(tokio::fs::File::from_std(file), initial_window_id);
        Ok(Self::spawn(runtime, thread_id, path, writer))
    }

    fn resume(
        runtime: &Handle,
        thread_id: &str,
        path: &Path,
        history_len: usize,
    ) -> io::Result<Self> {
        let state = read_resume_writer_state(path, thread_id)?;
        if state.written_len > history_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Codex rollout contains history newer than the durable Nanocodex boundary",
            ));
        }
        let file = File::options().read(true).append(true).open(path)?;
        let writer = RolloutWriter::resumed(tokio::fs::File::from_std(file), state);
        Ok(Self::spawn(runtime, thread_id, path.to_path_buf(), writer))
    }

    fn spawn(runtime: &Handle, thread_id: &str, path: PathBuf, writer: RolloutWriter) -> Self {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let writer_path = path.clone();
        drop(runtime.spawn(async move {
            if let Err(source) = writer.run(receiver).await {
                error!(
                    target: "nanocodex",
                    rollout_path = %writer_path.display(),
                    error = %source,
                    "Codex rollout writer stopped"
                );
            }
        }));
        Self {
            info: RolloutInfo {
                thread_id: thread_id.to_owned(),
                path,
            },
            commands,
        }
    }

    pub(crate) fn info(&self) -> &RolloutInfo {
        &self.info
    }

    pub(crate) async fn persist(
        &self,
        session: &CommittedSession,
        turn: RolloutTurn,
    ) -> io::Result<()> {
        self.persist_commit(RolloutCommit::from_session(session, turn))
            .await
    }

    async fn persist_commit(&self, commit: RolloutCommit) -> io::Result<()> {
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(RolloutCommand::Commit {
                commit: Box::new(commit),
                result,
            })
            .await
            .map_err(|_| io::Error::other("Codex rollout writer stopped"))?;
        receiver
            .await
            .map_err(|_| io::Error::other("Codex rollout writer stopped"))?
    }

    #[cfg(test)]
    async fn persist_history(
        &self,
        history: ResponseHistory,
        revision: u64,
        turn: RolloutTurn,
    ) -> io::Result<()> {
        self.persist_commit(RolloutCommit::from_history(history, revision, turn))
            .await
    }

    pub(crate) async fn flush(&self) -> io::Result<()> {
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(RolloutCommand::Flush { result })
            .await
            .map_err(|_| io::Error::other("Codex rollout writer stopped"))?;
        receiver
            .await
            .map_err(|_| io::Error::other("Codex rollout writer stopped"))?
    }
}

struct RolloutWriter {
    file: tokio::fs::File,
    pending: Option<RolloutCommit>,
    written_revision: Option<u64>,
    written_len: usize,
    window_number: u64,
    first_window_id: String,
    current_window_id: String,
    #[cfg(test)]
    injected_write_failures: usize,
}

impl RolloutWriter {
    fn new(file: tokio::fs::File, initial_window_id: String) -> Self {
        Self {
            file,
            pending: None,
            written_revision: None,
            written_len: 0,
            window_number: 0,
            first_window_id: initial_window_id.clone(),
            current_window_id: initial_window_id,
            #[cfg(test)]
            injected_write_failures: 0,
        }
    }

    fn resumed(file: tokio::fs::File, state: ResumeWriterState) -> Self {
        Self {
            file,
            pending: None,
            written_revision: Some(0),
            written_len: state.written_len,
            window_number: state.window_number,
            first_window_id: state.first_window_id,
            current_window_id: state.current_window_id,
            #[cfg(test)]
            injected_write_failures: 0,
        }
    }

    async fn run(mut self, mut commands: mpsc::Receiver<RolloutCommand>) -> io::Result<()> {
        while let Some(command) = commands.recv().await {
            match command {
                RolloutCommand::Commit { commit, result } => {
                    self.pending = Some(*commit);
                    drop(result.send(self.persist_pending().await));
                }
                RolloutCommand::Flush { result } => {
                    drop(result.send(self.flush().await));
                }
            }
        }
        self.flush().await
    }

    async fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_some() {
            self.persist_pending().await
        } else {
            self.file.flush().await?;
            self.file.sync_data().await
        }
    }

    async fn persist_pending(&mut self) -> io::Result<()> {
        let Some(commit) = self.pending.take() else {
            return Ok(());
        };
        let persisted = self.append_with_retry(&commit).await;
        match persisted {
            Ok(()) => Ok(()),
            Err(source) => {
                self.pending = Some(commit);
                Err(source)
            }
        }
    }

    async fn append_with_retry(&mut self, commit: &RolloutCommit) -> io::Result<()> {
        let prepared = self.prepare_append(commit)?;
        let original_len = self.file.metadata().await?.len();
        let first_error = match self.write_prepared(&prepared).await {
            Ok(()) => {
                self.apply_prepared(prepared);
                return Ok(());
            }
            Err(source) => source,
        };

        self.rollback(original_len).await?;
        match self.write_prepared(&prepared).await {
            Ok(()) => {
                self.apply_prepared(prepared);
                Ok(())
            }
            Err(second) => {
                self.rollback(original_len).await?;
                Err(io::Error::new(
                    second.kind(),
                    format!(
                        "failed to append Codex rollout after retry; first error: {first_error}; final error: {second}"
                    ),
                ))
            }
        }
    }

    fn prepare_append(&self, commit: &RolloutCommit) -> io::Result<PreparedAppend> {
        let len = commit.history.len();
        match self.written_revision {
            None => Ok(PreparedAppend {
                records: PreparedRecords::Items {
                    history: commit.history.clone(),
                    start: 0,
                },
                revision: commit.revision,
                len,
                window: None,
                turn: commit.turn.clone(),
            }),
            Some(revision) if revision == commit.revision => {
                if len < self.written_len {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "committed history shrank without a compaction revision",
                    ));
                }
                Ok(PreparedAppend {
                    records: PreparedRecords::Items {
                        history: commit.history.clone(),
                        start: self.written_len,
                    },
                    revision,
                    len,
                    window: None,
                    turn: commit.turn.clone(),
                })
            }
            Some(_) => {
                let window_number = self.window_number.saturating_add(1);
                let window_id = uuid::Uuid::now_v7().to_string();
                Ok(PreparedAppend {
                    records: PreparedRecords::Compacted(CompactedItem {
                        message: String::new(),
                        replacement_history: commit.history.iter().cloned().collect(),
                        window_number,
                        first_window_id: self.first_window_id.clone(),
                        previous_window_id: self.current_window_id.clone(),
                        window_id: window_id.clone(),
                    }),
                    revision: commit.revision,
                    len,
                    window: Some(WindowAdvance {
                        number: window_number,
                        id: window_id,
                    }),
                    turn: commit.turn.clone(),
                })
            }
        }
    }

    async fn write_prepared(&mut self, prepared: &PreparedAppend) -> io::Result<()> {
        #[cfg(test)]
        if self.injected_write_failures > 0 {
            self.injected_write_failures -= 1;
            return Err(io::Error::other("injected rollout write failure"));
        }
        self.write_event(CodexEvent::TaskStarted {
            turn_id: &prepared.turn.turn_id,
            started_at: prepared.turn.started_at,
            model_context_window: None,
            collaboration_mode_kind: "default",
        })
        .await?;
        self.write_event(CodexEvent::UserMessage(&prepared.turn.user_message))
            .await?;

        match &prepared.records {
            PreparedRecords::Items { history, start } => {
                for item in history.iter_from(*start) {
                    write_async_line(
                        &mut self.file,
                        &RolloutLine {
                            timestamp: timestamp(),
                            item: RolloutItem::ResponseItem(item),
                        },
                    )
                    .await?;
                }
            }
            PreparedRecords::Compacted(compacted) => {
                write_async_line(
                    &mut self.file,
                    &RolloutLine {
                        timestamp: timestamp(),
                        item: RolloutItem::Compacted(compacted),
                    },
                )
                .await?;
            }
        }
        if let Some(message) = prepared.turn.final_message.as_deref() {
            self.write_event(CodexEvent::AgentMessage {
                message,
                phase: "final_answer",
                memory_citation: None,
            })
            .await?;
        }
        match prepared.turn.status {
            RolloutTurnStatus::Completed => {
                self.write_event(CodexEvent::TaskComplete {
                    turn_id: &prepared.turn.turn_id,
                    last_agent_message: prepared.turn.final_message.as_deref(),
                    started_at: prepared.turn.started_at,
                    completed_at: prepared.turn.completed_at,
                    duration_ms: prepared.turn.duration_ms,
                    time_to_first_token_ms: None,
                })
                .await?;
            }
            RolloutTurnStatus::Interrupted => {
                self.write_event(CodexEvent::TurnAborted {
                    turn_id: &prepared.turn.turn_id,
                    reason: "interrupted",
                    started_at: prepared.turn.started_at,
                    completed_at: prepared.turn.completed_at,
                    duration_ms: prepared.turn.duration_ms,
                })
                .await?;
            }
            RolloutTurnStatus::InProgress => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot persist an unfinished Codex rollout turn",
                ));
            }
        }
        self.file.flush().await?;
        self.file.sync_data().await
    }

    async fn write_event(&mut self, event: CodexEvent<'_>) -> io::Result<()> {
        write_async_line(
            &mut self.file,
            &RolloutLine {
                timestamp: timestamp(),
                item: RolloutItem::Event(&event),
            },
        )
        .await
    }

    fn apply_prepared(&mut self, prepared: PreparedAppend) {
        self.written_revision = Some(prepared.revision);
        self.written_len = prepared.len;
        if let Some(window) = prepared.window {
            self.window_number = window.number;
            self.current_window_id = window.id;
        }
    }

    async fn rollback(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len).await?;
        self.file.seek(std::io::SeekFrom::Start(len)).await?;
        self.file.sync_data().await
    }

    #[cfg(test)]
    fn inject_write_failures(&mut self, count: usize) {
        self.injected_write_failures = count;
    }
}

struct PreparedAppend {
    records: PreparedRecords,
    revision: u64,
    len: usize,
    window: Option<WindowAdvance>,
    turn: RolloutTurn,
}

enum PreparedRecords {
    Items {
        history: ResponseHistory,
        start: usize,
    },
    Compacted(CompactedItem),
}

struct WindowAdvance {
    number: u64,
    id: String,
}

struct ResumeWriterState {
    written_len: usize,
    window_number: u64,
    first_window_id: String,
    current_window_id: String,
}

#[derive(Serialize)]
struct RolloutLine<T> {
    timestamp: String,
    #[serde(flatten)]
    item: T,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
enum RolloutItem<'a> {
    SessionMeta(&'a SessionMeta),
    #[serde(rename = "event_msg")]
    Event(&'a CodexEvent<'a>),
    ResponseItem(&'a ResponseItem),
    Compacted(&'a CompactedItem),
}

#[derive(Clone, Serialize)]
struct UserMessage {
    message: String,
    images: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    image_details: Vec<Option<ImageDetail>>,
    local_images: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    local_image_details: Vec<Option<ImageDetail>>,
    text_elements: Vec<serde_json::Value>,
}

impl UserMessage {
    fn from_prompt(prompt: &Prompt) -> Self {
        let mut message = String::new();
        let mut images = Vec::new();
        let mut image_details = Vec::new();
        let mut local_images = Vec::new();
        let mut local_image_details = Vec::new();
        match &prompt.instruction {
            PromptInput::Text(text) => message.push_str(text),
            PromptInput::Content(items) => {
                for item in items {
                    match item {
                        UserInput::Text { text } => message.push_str(text),
                        UserInput::Image { image_url, detail } => {
                            images.push(image_url.clone());
                            image_details.push(*detail);
                        }
                        UserInput::LocalImage { path, detail } => {
                            local_images.push(path.clone());
                            local_image_details.push(*detail);
                        }
                        UserInput::Audio { audio_url } => {
                            message.push_str("[Audio: ");
                            message.push_str(audio_url);
                            message.push(']');
                        }
                        UserInput::LocalAudio { path } => {
                            message.push_str("[Audio: ");
                            message.push_str(&path.display().to_string());
                            message.push(']');
                        }
                    }
                }
            }
        }
        Self {
            message,
            images,
            image_details,
            local_images,
            local_image_details,
            text_elements: Vec::new(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexEvent<'a> {
    TaskStarted {
        turn_id: &'a str,
        started_at: i64,
        model_context_window: Option<i64>,
        collaboration_mode_kind: &'static str,
    },
    UserMessage(&'a UserMessage),
    AgentMessage {
        message: &'a str,
        phase: &'static str,
        memory_citation: Option<serde_json::Value>,
    },
    TaskComplete {
        turn_id: &'a str,
        last_agent_message: Option<&'a str>,
        started_at: i64,
        completed_at: Option<i64>,
        duration_ms: Option<i64>,
        time_to_first_token_ms: Option<i64>,
    },
    TurnAborted {
        turn_id: &'a str,
        reason: &'static str,
        started_at: i64,
        completed_at: Option<i64>,
        duration_ms: Option<i64>,
    },
}

#[derive(Serialize)]
struct BaseInstructions {
    text: String,
}

#[derive(Serialize)]
struct SessionContextWindow {
    window_id: String,
}

#[derive(Serialize)]
struct SessionMeta {
    session_id: String,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    forked_from_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_thread_id: Option<String>,
    timestamp: String,
    cwd: PathBuf,
    originator: String,
    cli_version: String,
    source: &'static str,
    thread_source: &'static str,
    model_provider: &'static str,
    base_instructions: BaseInstructions,
    history_mode: &'static str,
    context_window: SessionContextWindow,
}

#[derive(Serialize)]
struct CompactedItem {
    message: String,
    replacement_history: Vec<ResponseItem>,
    window_number: u64,
    first_window_id: String,
    previous_window_id: String,
    window_id: String,
}

fn read_resume_writer_state(path: &Path, thread_id: &str) -> io::Result<ResumeWriterState> {
    let mut first_window_id = None;
    let mut current_window_id = None;
    let mut window_number = 0;
    let mut written_len = 0;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let value: serde_json::Value = serde_json::from_str(&line).map_err(io::Error::other)?;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("session_meta") if first_window_id.is_none() => {
                let payload = &value["payload"];
                if payload.get("id").and_then(serde_json::Value::as_str) != Some(thread_id) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Codex rollout thread ID does not match durable session",
                    ));
                }
                let window = payload["context_window"]["window_id"]
                    .as_str()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Codex rollout is missing its initial context window",
                        )
                    })?
                    .to_owned();
                first_window_id = Some(window.clone());
                current_window_id = Some(window);
            }
            Some("compacted") => {
                let payload = &value["payload"];
                written_len = payload["replacement_history"]
                    .as_array()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Codex compaction is missing replacement history",
                        )
                    })?
                    .len();
                window_number = payload["window_number"].as_u64().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Codex compaction is missing its window number",
                    )
                })?;
                current_window_id = Some(
                    payload["window_id"]
                        .as_str()
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Codex compaction is missing its window ID",
                            )
                        })?
                        .to_owned(),
                );
            }
            Some("response_item") => written_len = written_len.saturating_add(1),
            _ => {}
        }
    }
    let first_window_id = first_window_id.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex rollout is missing session metadata",
        )
    })?;
    Ok(ResumeWriterState {
        written_len,
        window_number,
        current_window_id: current_window_id.unwrap_or_else(|| first_window_id.clone()),
        first_window_id,
    })
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn write_line(output: &mut impl Write, line: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *output, line).map_err(io::Error::other)?;
    output.write_all(b"\n")
}

async fn write_async_line(output: &mut tokio::fs::File, line: &impl Serialize) -> io::Result<()> {
    let mut encoded = serde_json::to_vec(line).map_err(io::Error::other)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read};

    use nanocodex_core::{ContentItem, MessageRole};
    use serde_json::Value;
    use tempfile::tempdir;

    use super::*;

    fn message(text: &str) -> ResponseItem {
        ResponseItem::message(
            MessageRole::User,
            [ContentItem::InputText { text: text.into() }],
        )
    }

    fn completed_turn(prompt: &str, final_message: &str) -> RolloutTurn {
        RolloutTurn::started(&Prompt::new(prompt)).completed(final_message.to_owned())
    }

    fn recorder(home: &Path) -> RolloutRecorder {
        RolloutRecorder::create(
            &Handle::current(),
            &RolloutConfig::new(home),
            "019c0d31-c308-7d91-bff4-5dca82d15ac6",
            Path::new("/worktree"),
            "base instructions",
            RolloutOrigin {
                kind: "root",
                parent_thread_id: None,
            },
            None,
        )
        .expect("create rollout")
    }

    fn lines(recorder: &RolloutRecorder) -> Vec<Value> {
        BufReader::new(File::open(recorder.info().path()).expect("open rollout"))
            .lines()
            .map(|line| serde_json::from_str(&line.expect("read line")).expect("parse line"))
            .collect()
    }

    #[test]
    fn loads_codex_rollout_without_a_nanocodex_sidecar() {
        let home = tempdir().expect("temporary Codex home");
        let thread_id = "019c0d31-c308-7d91-bff4-5dca82d15ac6";
        let directory = home.path().join("sessions/2026/07/24");
        std::fs::create_dir_all(&directory).expect("create rollout directory");
        let path = directory.join(format!("rollout-2026-07-24T12-00-00-{thread_id}.jsonl"));
        let mut file = File::create(&path).expect("create Codex rollout");
        for value in [
            serde_json::json!({
                "timestamp": "2026-07-24T12:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": thread_id,
                    "cwd": home.path(),
                    "originator": "codex-tui",
                    "history_mode": "legacy",
                    "context_window": {"window_id": "window-1"}
                }
            }),
            serde_json::json!({
                "timestamp": "2026-07-24T12:00:01Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "visible prompt"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-24T12:00:01Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "discarded"}]
                }
            }),
            serde_json::json!({
                "timestamp": "2026-07-24T12:00:02Z",
                "type": "compacted",
                "payload": {
                    "replacement_history": [{
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "retained"}]
                    }],
                    "window_number": 1,
                    "first_window_id": "window-1",
                    "previous_window_id": "window-1",
                    "window_id": "window-2"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-07-24T12:00:03Z",
                "type": "event_msg",
                "payload": {"type": "agent_reasoning", "text": "checking the workspace"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-24T12:00:03Z",
                "type": "event_msg",
                "payload": {"type": "agent_message", "message": "visible answer"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-24T12:00:03Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "continued"}]
                }
            }),
        ] {
            write_line(&mut file, &value).expect("write rollout line");
        }
        file.flush().expect("flush rollout");

        let session = RolloutConfig::new(home.path())
            .load_session(thread_id)
            .expect("load Codex rollout");
        assert_eq!(
            Path::new(session.workspace()).canonicalize().unwrap(),
            home.path().canonicalize().unwrap()
        );
        assert_eq!(session.rollout_path(), path.canonicalize().unwrap());
        let snapshot = serde_json::to_value(session.snapshot()).expect("encode snapshot");
        assert!(snapshot.get("request_prefix").is_none());
        assert_eq!(snapshot["history"].as_array().map(Vec::len), Some(2));
        let history = snapshot["history"].to_string();
        assert!(history.contains("retained"));
        assert!(history.contains("continued"));
        assert!(!history.contains("discarded"));
        assert_eq!(
            session.transcript(),
            [
                RolloutTranscriptItem::User("visible prompt".to_owned()),
                RolloutTranscriptItem::Reasoning("checking the workspace".to_owned()),
                RolloutTranscriptItem::Assistant("visible answer".to_owned()),
            ]
        );
    }

    #[test]
    fn reconstructs_custom_function_and_mcp_tool_activity() {
        assert_eq!(
            visible_tool_call(&serde_json::json!({
                "type": "custom_tool_call",
                "call_id": "custom-1",
                "name": "exec",
                "input": "text(true);"
            })),
            Some(RolloutTranscriptItem::Tool {
                call_id: "custom-1".to_owned(),
                name: "exec".to_owned(),
                arguments: "text(true);".to_owned(),
            })
        );
        assert_eq!(
            visible_tool_call(&serde_json::json!({
                "type": "function_call",
                "call_id": "function-1",
                "name": "wait",
                "arguments": "{\"cell_id\":\"1\"}"
            })),
            Some(RolloutTranscriptItem::Tool {
                call_id: "function-1".to_owned(),
                name: "wait".to_owned(),
                arguments: "{\"cell_id\":\"1\"}".to_owned(),
            })
        );
        assert_eq!(
            visible_rollout_event(&serde_json::json!({
                "type": "mcp_tool_call_end",
                "call_id": "mcp-1",
                "invocation": {
                    "server": "node_repl",
                    "tool": "js",
                    "arguments": {"code": "return true"}
                }
            })),
            Some(RolloutTranscriptItem::Tool {
                call_id: "mcp-1".to_owned(),
                name: "node_repl.js".to_owned(),
                arguments: "{\"code\":\"return true\"}".to_owned(),
            })
        );
        assert_eq!(
            visible_rollout_event(&serde_json::json!({
                "type": "web_search_end",
                "call_id": "search-1",
                "query": "Nanocodex",
                "action": {"type": "search", "queries": ["Nanocodex"]}
            })),
            Some(RolloutTranscriptItem::Tool {
                call_id: "search-1".to_owned(),
                name: "web_search".to_owned(),
                arguments: "{\"queries\":[\"Nanocodex\"],\"type\":\"search\"}".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn writes_codex_rollout_envelope_and_committed_items() {
        let home = tempdir().expect("temporary Codex home");
        let recorder = recorder(home.path());
        recorder
            .persist_history(
                ResponseHistory::new(vec![message("remember amber")]),
                0,
                completed_turn("remember amber", "stored"),
            )
            .await
            .expect("persist rollout");

        let lines = lines(&recorder);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0]["type"], "session_meta");
        assert_eq!(
            lines[0]["payload"]["id"],
            "019c0d31-c308-7d91-bff4-5dca82d15ac6"
        );
        assert_eq!(lines[0]["payload"]["source"], "cli");
        assert_eq!(lines[0]["payload"]["history_mode"], "legacy");
        assert!(lines[0]["payload"]["context_window"]["window_id"].is_string());
        assert_eq!(lines[1]["type"], "event_msg");
        assert_eq!(lines[1]["payload"]["type"], "task_started");
        assert_eq!(lines[2]["payload"]["type"], "user_message");
        assert_eq!(lines[2]["payload"]["message"], "remember amber");
        assert_eq!(lines[3]["type"], "response_item");
        assert_eq!(lines[3]["payload"]["type"], "message");
        assert_eq!(lines[3]["payload"]["role"], "user");
        assert_eq!(lines[4]["payload"]["type"], "agent_message");
        assert_eq!(lines[4]["payload"]["message"], "stored");
        assert_eq!(lines[4]["payload"]["phase"], "final_answer");
        assert_eq!(lines[5]["payload"]["type"], "task_complete");
        assert_eq!(
            lines[5]["payload"]["turn_id"],
            lines[1]["payload"]["turn_id"]
        );
    }

    #[tokio::test]
    async fn appends_only_the_new_committed_delta() {
        let home = tempdir().expect("temporary Codex home");
        let recorder = recorder(home.path());
        recorder
            .persist_history(
                ResponseHistory::new(vec![message("one")]),
                0,
                completed_turn("one", "first"),
            )
            .await
            .expect("persist first turn");
        let mut prefix = Vec::new();
        File::open(recorder.info().path())
            .expect("open rollout")
            .read_to_end(&mut prefix)
            .expect("read prefix");

        recorder
            .persist_history(
                ResponseHistory::new(vec![message("one"), message("two")]),
                0,
                completed_turn("two", "second"),
            )
            .await
            .expect("persist second turn");

        let mut complete = Vec::new();
        File::open(recorder.info().path())
            .expect("open rollout")
            .read_to_end(&mut complete)
            .expect("read complete rollout");
        assert!(complete.starts_with(&prefix));
        let lines = lines(&recorder);
        assert_eq!(lines.len(), 11);
        assert_eq!(lines[7]["payload"]["message"], "two");
        assert_eq!(lines[8]["type"], "response_item");
        assert_eq!(lines[9]["payload"]["message"], "second");
    }

    #[tokio::test]
    async fn resumed_writer_repairs_a_rollout_behind_the_durable_boundary() {
        let home = tempdir().expect("temporary Codex home");
        let original = recorder(home.path());
        original
            .persist_history(
                ResponseHistory::new(vec![message("one")]),
                0,
                completed_turn("one", "first"),
            )
            .await
            .expect("persist first turn");
        original.flush().await.expect("flush first turn");
        let path = original.info().path().to_path_buf();
        drop(original);

        let config = RolloutConfig::new(home.path()).resumed(path.clone());
        let resumed = RolloutRecorder::create(
            &Handle::current(),
            &config,
            "019c0d31-c308-7d91-bff4-5dca82d15ac6",
            Path::new("/worktree"),
            "base instructions",
            RolloutOrigin {
                kind: "resume",
                parent_thread_id: None,
            },
            // The durable snapshot already contains `two`, but its rollout append failed.
            Some(2),
        )
        .expect("resume rollout");
        resumed
            .persist_history(
                ResponseHistory::new(vec![message("one"), message("two"), message("three")]),
                0,
                completed_turn("three", "resumed"),
            )
            .await
            .expect("persist resumed turn");

        let lines = lines(&resumed);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line["type"] == "session_meta")
                .count(),
            1
        );
        let response_text = lines
            .iter()
            .filter(|line| line["type"] == "response_item")
            .map(|line| line["payload"]["content"][0]["text"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(response_text, ["one", "two", "three"]);
    }

    #[tokio::test]
    async fn records_compaction_as_a_replacement_history_boundary() {
        let home = tempdir().expect("temporary Codex home");
        let recorder = recorder(home.path());
        recorder
            .persist_history(
                ResponseHistory::new(vec![message("one"), message("two")]),
                0,
                completed_turn("two", "before compaction"),
            )
            .await
            .expect("persist original history");
        recorder
            .persist_history(
                ResponseHistory::new(vec![message("summary")]),
                1,
                completed_turn("continue", "after compaction"),
            )
            .await
            .expect("persist compaction");
        recorder.flush().await.expect("flush rollout");

        let lines = lines(&recorder);
        let compacted = lines
            .iter()
            .find(|line| line["type"] == "compacted")
            .expect("compacted record");
        assert_eq!(lines.len(), 12);
        assert_eq!(compacted["payload"]["window_number"], 1);
        assert_eq!(
            compacted["payload"]["replacement_history"][0]["content"][0]["text"],
            "summary"
        );
    }

    #[tokio::test]
    async fn fork_metadata_retains_parent_identity() {
        let home = tempdir().expect("temporary Codex home");
        let parent = "019c0d31-c308-7d91-bff4-5dca82d15ac5";
        let recorder = RolloutRecorder::create(
            &Handle::current(),
            &RolloutConfig::new(home.path()),
            "019c0d31-c308-7d91-bff4-5dca82d15ac6",
            Path::new("/worktree"),
            "base instructions",
            RolloutOrigin {
                kind: "fork",
                parent_thread_id: Some(parent),
            },
            None,
        )
        .expect("create fork rollout");

        let lines = lines(&recorder);
        assert_eq!(lines[0]["payload"]["forked_from_id"], parent);
        assert_eq!(lines[0]["payload"]["parent_thread_id"], parent);
    }

    #[tokio::test]
    async fn failed_append_remains_pending_and_retries_without_duplicates() {
        let home = tempdir().expect("temporary rollout directory");
        let path = home.path().join("rollout.jsonl");
        let file = File::create(&path).expect("create temporary rollout");
        let mut writer = RolloutWriter::new(
            tokio::fs::File::from_std(file),
            uuid::Uuid::now_v7().to_string(),
        );
        writer.pending = Some(RolloutCommit::from_history(
            ResponseHistory::new(vec![message("retry me")]),
            0,
            completed_turn("retry me", "retried"),
        ));
        writer.inject_write_failures(2);

        assert!(writer.persist_pending().await.is_err());
        assert!(writer.pending.is_some());
        writer.flush().await.expect("retry pending append");
        drop(writer);

        let lines = BufReader::new(File::open(path).expect("open retried rollout"))
            .lines()
            .collect::<io::Result<Vec<_>>>()
            .expect("read retried rollout");
        assert_eq!(lines.len(), 5);
    }
}
