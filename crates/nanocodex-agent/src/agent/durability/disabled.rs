use crate::{Result, session::CommittedSession};

#[derive(Clone, Default)]
pub(crate) struct DurabilityConfig;

impl DurabilityConfig {
    pub(crate) const fn for_new_thread(&self) -> Self {
        Self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        &self,
        _session_id: &str,
        _workspace: Option<&str>,
        _instructions: &str,
        _origin_kind: &'static str,
        _parent_session_id: Option<&str>,
        _resume_history_len: Option<usize>,
    ) -> Result<Durability> {
        Ok(Durability)
    }
}

#[derive(Clone)]
pub(crate) struct Durability;

impl Durability {
    pub(crate) const fn start_turn(&self, _prompt: &nanocodex_oai_api::Prompt) -> DurabilityTurn {
        DurabilityTurn
    }

    pub(crate) async fn persist(&self, _checkpoint: &CommittedSession, _turn: DurabilityTurn) {}
}

pub(crate) struct DurabilityTurn;

impl DurabilityTurn {
    pub(crate) fn completed(self, _final_message: String) -> Self {
        self
    }

    pub(crate) const fn interrupted(self) -> Self {
        self
    }
}
