pub(in crate::rollout) mod writer;

use super::wire::*;
use super::*;
use writer::*;

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

#[derive(Clone, Debug)]
pub(crate) struct RolloutRecorder {
    info: RolloutInfo,
    unpublished_path: Option<PathBuf>,
    commands: mpsc::Sender<RolloutCommand>,
}

#[derive(Clone, Copy)]
pub(crate) struct RolloutOrigin<'a> {
    pub(crate) kind: &'a str,
    pub(crate) parent_thread_id: Option<&'a str>,
}

/// Nanocodex identity retained in otherwise Codex-compatible session metadata.
#[derive(Clone, Copy)]
pub(crate) struct RolloutIdentity<'a> {
    pub(crate) lineage_id: &'a str,
    pub(crate) prompt_cache_key: &'a str,
}

enum RolloutCommand {
    SeedInitialCheckpoint {
        seed: Box<RolloutSeed>,
        result: oneshot::Sender<io::Result<()>>,
    },
    Commit {
        commit: Box<RolloutCommit>,
        result: oneshot::Sender<io::Result<()>>,
    },
    Flush {
        result: oneshot::Sender<io::Result<()>>,
    },
    Publish {
        path: PathBuf,
        result: oneshot::Sender<io::Result<()>>,
    },
    Shutdown {
        result: oneshot::Sender<io::Result<()>>,
    },
    #[cfg(test)]
    InjectWriteFailures {
        count: usize,
        result: oneshot::Sender<()>,
    },
    #[cfg(test)]
    InjectWriteFailureAfter {
        writes: usize,
        result: oneshot::Sender<()>,
    },
    #[cfg(test)]
    InjectPublishReopenFailure { result: oneshot::Sender<()> },
    #[cfg(test)]
    InjectRollbackFailure { result: oneshot::Sender<()> },
}

pub(super) struct RolloutCommit {
    history: ResponseHistory,
    revision: u64,
    turn: RolloutTurn,
    model: Model,
    context_baseline: ContextBaseline,
}

pub(super) struct RolloutSeed {
    history: ResponseHistory,
    revision: u64,
    model: Model,
    context_baseline: ContextBaseline,
    effort: Thinking,
}

impl RolloutSeed {
    fn from_session(session: &CommittedSession, effort: Thinking) -> Self {
        Self {
            history: session.rollout_history(),
            revision: session.history_revision(),
            model: session.selected_model(),
            context_baseline: session.context_baseline().clone(),
            effort,
        }
    }

    #[cfg(test)]
    pub(super) const fn from_history(
        history: ResponseHistory,
        revision: u64,
        effort: Thinking,
    ) -> Self {
        Self {
            history,
            revision,
            model: Model::Sol,
            context_baseline: ContextBaseline::Missing,
            effort,
        }
    }
}

impl RolloutCommit {
    fn from_session(session: &CommittedSession, turn: RolloutTurn) -> Self {
        Self {
            history: session.rollout_history(),
            revision: session.history_revision(),
            turn,
            model: session.selected_model(),
            context_baseline: session.context_baseline().clone(),
        }
    }

    fn compaction(session: &CommittedSession, turn: RolloutTurn) -> Self {
        Self {
            history: session.rollout_history(),
            revision: session.history_revision(),
            turn,
            model: session.selected_model(),
            context_baseline: session.context_baseline().clone(),
        }
    }

    #[cfg(test)]
    pub(super) const fn from_history(
        history: ResponseHistory,
        revision: u64,
        turn: RolloutTurn,
    ) -> Self {
        Self {
            history,
            revision,
            turn,
            model: Model::Sol,
            context_baseline: ContextBaseline::Missing,
        }
    }
}

#[derive(Clone)]
pub(crate) struct RolloutTurn {
    turn_id: String,
    user_message: Option<UserMessage>,
    final_message: Option<String>,
    started_at: i64,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    status: RolloutTurnStatus,
    effort: Thinking,
    timer: Option<Instant>,
}

#[derive(Clone, Copy)]
enum RolloutTurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Replaced,
    Failed,
}

impl RolloutTurn {
    pub(crate) fn started(prompt: &Prompt, effort: Thinking) -> Self {
        Self {
            turn_id: uuid::Uuid::now_v7().to_string(),
            user_message: Some(UserMessage::from_prompt(prompt)),
            final_message: None,
            started_at: Utc::now().timestamp(),
            completed_at: None,
            duration_ms: None,
            status: RolloutTurnStatus::InProgress,
            effort,
            timer: Some(Instant::now()),
        }
    }

    pub(crate) fn compaction_started(effort: Thinking) -> Self {
        Self {
            turn_id: uuid::Uuid::now_v7().to_string(),
            user_message: None,
            final_message: None,
            started_at: Utc::now().timestamp(),
            completed_at: None,
            duration_ms: None,
            status: RolloutTurnStatus::InProgress,
            effort,
            timer: Some(Instant::now()),
        }
    }

    pub(crate) fn completed(mut self, final_message: String) -> Self {
        self.finish(RolloutTurnStatus::Completed);
        self.final_message = Some(final_message);
        self
    }

    pub(crate) fn completed_without_message(mut self) -> Self {
        self.finish(RolloutTurnStatus::Completed);
        self
    }

    pub(crate) fn interrupted(mut self) -> Self {
        self.finish(RolloutTurnStatus::Interrupted);
        self
    }

    pub(crate) fn replaced(mut self) -> Self {
        self.finish(RolloutTurnStatus::Replaced);
        self
    }

    pub(crate) fn failed(mut self) -> Self {
        self.finish(RolloutTurnStatus::Failed);
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create(
        runtime: &Handle,
        config: &RolloutConfig,
        thread_id: &str,
        identity: RolloutIdentity<'_>,
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
        let unpublished_path = (origin.kind == "fork")
            .then(|| directory.join(format!(".rollout-{filename_timestamp}-{thread_id}.pending")));
        let writer_path = unpublished_path.as_ref().unwrap_or(&path);
        let initial_window_id = uuid::Uuid::now_v7().to_string();
        let timestamp = timestamp();
        let parent_thread_id = origin.parent_thread_id.map(ToOwned::to_owned);
        let meta = SessionMeta {
            session_id: thread_id.to_owned(),
            id: thread_id.to_owned(),
            nanocodex_lineage_id: identity.lineage_id.to_owned(),
            nanocodex_prompt_cache_key: identity.prompt_cache_key.to_owned(),
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
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(writer_path)?;
        write_line(
            &mut file,
            &RolloutLine {
                timestamp,
                item: RolloutItem::SessionMeta(&meta),
            },
        )?;
        file.flush()?;
        file.sync_all()?;

        let writer = RolloutWriter::new(
            tokio::fs::File::from_std(file),
            writer_path.to_path_buf(),
            unpublished_path.is_some(),
            initial_window_id,
            cwd.to_path_buf(),
        );
        #[cfg(test)]
        let mut writer = writer;
        #[cfg(test)]
        if config.fail_fork_seed && origin.kind == "fork" {
            writer.inject_write_failures(1);
        }
        #[cfg(test)]
        if config.fail_fork_publish && origin.kind == "fork" {
            writer.inject_publish_reopen_failure();
        }
        Ok(Self::spawn(
            runtime,
            thread_id,
            path,
            unpublished_path,
            writer,
        ))
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
        let writer =
            RolloutWriter::resumed(tokio::fs::File::from_std(file), path.to_path_buf(), state);
        Ok(Self::spawn(
            runtime,
            thread_id,
            path.to_path_buf(),
            None,
            writer,
        ))
    }

    fn spawn(
        runtime: &Handle,
        thread_id: &str,
        path: PathBuf,
        unpublished_path: Option<PathBuf>,
        writer: RolloutWriter,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let writer_path = path.clone();
        drop(runtime.spawn(async move {
            let (outcome, shutdown) = writer.run(receiver).await;
            if let Err(source) = &outcome {
                error!(
                    target: "nanocodex",
                    rollout_path = %writer_path.display(),
                    error = %source,
                    "Codex rollout writer stopped"
                );
            }
            if let Some(result) = shutdown {
                drop(result.send(outcome));
            }
        }));
        Self {
            info: RolloutInfo {
                thread_id: thread_id.to_owned(),
                path,
            },
            unpublished_path,
            commands,
        }
    }

    pub(crate) const fn info(&self) -> &RolloutInfo {
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

    pub(crate) async fn persist_compaction(
        &self,
        session: &CommittedSession,
        turn: RolloutTurn,
    ) -> io::Result<()> {
        self.persist_commit(RolloutCommit::compaction(session, turn))
            .await
    }

    pub(crate) async fn seed_initial_checkpoint(
        &self,
        checkpoint: &CommittedSession,
        effort: Thinking,
    ) -> io::Result<()> {
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(RolloutCommand::SeedInitialCheckpoint {
                seed: Box::new(RolloutSeed::from_session(checkpoint, effort)),
                result,
            })
            .await
            .map_err(|_| io::Error::other("Codex rollout writer stopped during initial seed"))?;
        receiver
            .await
            .map_err(|_| io::Error::other("Codex rollout writer stopped during initial seed"))??;
        self.publish().await
    }

    #[cfg(test)]
    pub(in crate::rollout) async fn seed_initial_history(
        &self,
        history: ResponseHistory,
        revision: u64,
        effort: Thinking,
    ) -> io::Result<()> {
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(RolloutCommand::SeedInitialCheckpoint {
                seed: Box::new(RolloutSeed::from_history(history, revision, effort)),
                result,
            })
            .await
            .expect("test rollout writer remains available");
        receiver
            .await
            .expect("test rollout writer accepts initial seed")?;
        self.publish().await
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
    pub(in crate::rollout) async fn persist_history(
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

    pub(crate) async fn shutdown(&self) -> io::Result<()> {
        let (result, receiver) = oneshot::channel();
        if self
            .commands
            .send(RolloutCommand::Shutdown { result })
            .await
            .is_err()
        {
            return Ok(());
        }
        let outcome = receiver
            .await
            .map_err(|_| io::Error::other("Codex rollout writer stopped during shutdown"))?;
        if let Some(path) = &self.unpublished_path {
            let _ = tokio::fs::remove_file(path).await;
        }
        outcome
    }

    pub(crate) async fn discard_unpublished_rollout(&self) {
        let Some(path) = &self.unpublished_path else {
            return;
        };
        let _ = tokio::fs::remove_file(path).await;
        let _ = tokio::fs::remove_file(&self.info.path).await;
    }

    async fn publish(&self) -> io::Result<()> {
        if self.unpublished_path.is_none() {
            return Ok(());
        }
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(RolloutCommand::Publish {
                path: self.info.path.clone(),
                result,
            })
            .await
            .map_err(|_| io::Error::other("Codex rollout writer stopped during publish"))?;
        receiver
            .await
            .map_err(|_| io::Error::other("Codex rollout writer stopped during publish"))?
    }

    #[cfg(test)]
    pub(crate) async fn inject_write_failures(&self, count: usize) {
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(RolloutCommand::InjectWriteFailures { count, result })
            .await
            .expect("test rollout writer remains available");
        receiver
            .await
            .expect("test rollout writer accepts injection");
    }

    #[cfg(test)]
    pub(crate) async fn inject_write_failure_after(&self, writes: usize) {
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(RolloutCommand::InjectWriteFailureAfter { writes, result })
            .await
            .expect("test rollout writer remains available");
        receiver
            .await
            .expect("test rollout writer accepts injection");
    }

    #[cfg(test)]
    pub(crate) async fn inject_publish_reopen_failure(&self) {
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(RolloutCommand::InjectPublishReopenFailure { result })
            .await
            .expect("test rollout writer remains available");
        receiver
            .await
            .expect("test rollout writer accepts injection");
    }

    #[cfg(test)]
    pub(in crate::rollout) async fn inject_rollback_failure(&self) {
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(RolloutCommand::InjectRollbackFailure { result })
            .await
            .expect("test rollout writer remains available");
        receiver
            .await
            .expect("test rollout writer accepts injection");
    }
}
