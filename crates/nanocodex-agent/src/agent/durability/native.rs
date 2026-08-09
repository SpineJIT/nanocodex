use std::path::{Path, PathBuf};

use crate::{
    NanocodexError, Result,
    error::PersistRolloutFailure,
    rollout::{
        RolloutConfig, RolloutIdentity, RolloutInfo, RolloutOrigin, RolloutRecorder, RolloutTurn,
    },
    session::CommittedSession,
};

#[derive(Clone, Default)]
pub(crate) struct DurabilityConfig {
    rollout: Option<RolloutConfig>,
}

impl DurabilityConfig {
    pub(crate) fn set_rollout(&mut self, rollout: RolloutConfig) {
        self.rollout = Some(rollout);
    }

    pub(crate) fn for_new_thread(&self) -> Self {
        Self {
            rollout: self.rollout.as_ref().map(RolloutConfig::for_new_thread),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        &self,
        session_id: &str,
        lineage_id: &str,
        prompt_cache_key: &str,
        workspace: Option<&str>,
        instructions: &str,
        origin_kind: &'static str,
        parent_session_id: Option<&str>,
        resume_history_len: Option<usize>,
    ) -> Result<Durability> {
        let Some(config) = &self.rollout else {
            return Ok(Durability::default());
        };
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| NanocodexError::TokioRuntimeUnavailable)?;
        let cwd =
            rollout_workspace(workspace).map_err(|source| NanocodexError::InitializeRollout {
                codex_home: config.codex_home().to_path_buf(),
                source,
            })?;
        let recorder = RolloutRecorder::create(
            &runtime,
            config,
            session_id,
            RolloutIdentity {
                lineage_id,
                prompt_cache_key,
            },
            &cwd,
            instructions,
            RolloutOrigin {
                kind: origin_kind,
                parent_thread_id: parent_session_id,
            },
            resume_history_len,
        )
        .map_err(|source| NanocodexError::InitializeRollout {
            codex_home: config.codex_home().to_path_buf(),
            source,
        })?;
        Ok(Durability {
            recorder: Some(recorder),
        })
    }
}

#[derive(Clone, Default)]
pub(crate) struct Durability {
    recorder: Option<RolloutRecorder>,
}

impl Durability {
    pub(crate) const fn info(&self) -> Option<&RolloutInfo> {
        match &self.recorder {
            Some(recorder) => Some(recorder.info()),
            None => None,
        }
    }

    pub(crate) fn start_turn(
        &self,
        prompt: &nanocodex_oai_api::Prompt,
        effort: nanocodex_oai_api::Thinking,
    ) -> DurabilityTurn {
        DurabilityTurn(
            self.recorder
                .as_ref()
                .map(|_| RolloutTurn::started(prompt, effort)),
        )
    }

    pub(crate) fn start_compaction(&self, effort: nanocodex_oai_api::Thinking) -> DurabilityTurn {
        DurabilityTurn(
            self.recorder
                .as_ref()
                .map(|_| RolloutTurn::compaction_started(effort)),
        )
    }

    pub(crate) async fn persist(
        &self,
        checkpoint: &CommittedSession,
        turn: DurabilityTurn,
    ) -> std::result::Result<(), PersistRolloutFailure> {
        let (Some(recorder), Some(turn)) = (&self.recorder, turn.0) else {
            return Ok(());
        };
        recorder.persist(checkpoint, turn).await.map_err(|source| {
            PersistRolloutFailure::new(recorder.info().path().to_path_buf(), source)
        })
    }

    pub(crate) async fn persist_compaction(
        &self,
        checkpoint: &CommittedSession,
        turn: DurabilityTurn,
    ) -> std::result::Result<(), PersistRolloutFailure> {
        let (Some(recorder), Some(turn)) = (&self.recorder, turn.0) else {
            return Ok(());
        };
        recorder
            .persist_compaction(checkpoint, turn)
            .await
            .map_err(|source| {
                PersistRolloutFailure::new(recorder.info().path().to_path_buf(), source)
            })
    }

    pub(crate) async fn seed_initial_checkpoint(
        &self,
        checkpoint: &CommittedSession,
        effort: nanocodex_oai_api::Thinking,
    ) -> std::result::Result<(), PersistRolloutFailure> {
        let Some(recorder) = &self.recorder else {
            return Ok(());
        };
        recorder
            .seed_initial_checkpoint(checkpoint, effort)
            .await
            .map_err(|source| {
                PersistRolloutFailure::new(recorder.info().path().to_path_buf(), source)
            })
    }

    pub(crate) async fn flush(&self) -> std::result::Result<(), PersistRolloutFailure> {
        let Some(recorder) = &self.recorder else {
            return Ok(());
        };
        recorder.flush().await.map_err(|source| {
            PersistRolloutFailure::new(recorder.info().path().to_path_buf(), source)
        })
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let Some(recorder) = &self.recorder else {
            return Ok(());
        };
        recorder
            .shutdown()
            .await
            .map_err(|source| NanocodexError::PersistRollout {
                path: recorder.info().path().to_path_buf(),
                source,
            })
    }

    #[cfg(test)]
    pub(crate) async fn inject_write_failures(&self, count: usize) {
        if let Some(recorder) = &self.recorder {
            recorder.inject_write_failures(count).await;
        }
    }
}

pub(crate) struct DurabilityTurn(Option<RolloutTurn>);

impl DurabilityTurn {
    pub(crate) fn completed(self, final_message: String) -> Self {
        Self(self.0.map(|turn| turn.completed(final_message)))
    }

    pub(crate) fn completed_without_message(self) -> Self {
        Self(self.0.map(RolloutTurn::completed_without_message))
    }

    pub(crate) fn interrupted(self) -> Self {
        Self(self.0.map(RolloutTurn::interrupted))
    }

    pub(crate) fn replaced(self) -> Self {
        Self(self.0.map(RolloutTurn::replaced))
    }

    pub(crate) fn failed(self) -> Self {
        Self(self.0.map(RolloutTurn::failed))
    }
}

fn rollout_workspace(workspace: Option<&str>) -> std::io::Result<PathBuf> {
    let current = std::env::current_dir()?;
    let Some(workspace) = workspace else {
        return Ok(current);
    };
    let workspace = Path::new(workspace);
    if workspace.is_absolute() {
        Ok(workspace.to_path_buf())
    } else {
        Ok(current.join(workspace))
    }
}
