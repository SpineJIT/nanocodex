use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use futures_util::Stream;
use nanocodex_oai_api::{
    ModelConfig, OpenAi, Prompt, ReasoningMode, Thinking,
    auth::OpenAiAuthMode,
    events::{AgentEvent, AgentEvents, EventSink},
    responses::ResponseItem,
    session::SessionId,
    tower::{
        MakeResponsesService, ResponsesAttempt, ResponsesClient, ResponsesServiceResponse,
        StandardServiceFactory,
    },
    transport::{ResponsesHistory, ResponsesTransport, TransportStats},
};
use nanocodex_tools::{Tools, ToolsBuildError};
use tokio::sync::{mpsc, oneshot, watch};
use tower::Service;
use tracing::{Instrument, error, info, info_span};

use crate::prompt_cache::{ModelPromptCache, SharedPromptCache};
use crate::{
    NanocodexError, Result,
    model::{
        load_global_instructions,
        run::{
            CompletedModelTurn, ModelCheckpoint, ModelRun, ModelTurnOutcome, PreparedCheckpoint,
            prepare_checkpoint, prepare_resumed_checkpoint, prepare_rollout_checkpoint,
        },
    },
    rollout::{RolloutConfig, RolloutInfo, RolloutOrigin, RolloutRecorder, RolloutTurn},
    session::{CommittedSession, SessionResume, SessionSnapshot},
    usage::TurnUsage,
};

const COMMAND_CAPACITY: usize = 8;
const STEER_CAPACITY: usize = 8;
const CODEX_THREAD_ID_ENV_VAR: &str = "CODEX_THREAD_ID";

type ServiceFactory<S> = Arc<dyn Fn() -> S + Send + Sync>;
type ToolsFactory =
    Arc<dyn Fn(AgentHandle) -> std::result::Result<Tools, ToolsBuildError> + Send + Sync>;

enum InitialResume {
    Exact(Box<ModelCheckpoint>),
    Rollout(Box<RolloutResume>),
}

struct RolloutResume {
    workspace: String,
    canonical_context: ResponseItem,
    history: Vec<ResponseItem>,
    prompt_cache_key: Arc<str>,
}

impl InitialResume {
    fn workspace(&self) -> &str {
        match self {
            Self::Exact(checkpoint) => checkpoint.workspace(),
            Self::Rollout(resume) => &resume.workspace,
        }
    }

    fn history_len(&self) -> usize {
        match self {
            Self::Exact(checkpoint) => checkpoint.history().len(),
            Self::Rollout(resume) => resume.history.len(),
        }
    }
}

#[derive(Clone)]
enum ToolsConfiguration {
    Shared(Tools),
    PerAgent(ToolsFactory),
}

impl ToolsConfiguration {
    fn materialize(&self, agent_handle: AgentHandle) -> Result<Tools> {
        match self {
            Self::Shared(tools) => Ok(tools.clone()),
            Self::PerAgent(factory) => factory(agent_handle).map_err(Into::into),
        }
    }
}

fn bind_agent_environment(tools: Tools, session_id: &str) -> Result<Tools> {
    tools
        .into_builder()
        .process_environment([(CODEX_THREAD_ID_ENV_VAR, session_id)])
        .build()
        .map_err(Into::into)
}

mod builder;
mod driver;
mod handle;
mod spawn;
mod turn;

pub use builder::NanocodexBuilder;
pub use handle::{AgentHandle, Nanocodex};
pub use turn::{Turn, TurnControl, TurnResult};

use builder::{CodexCompatibility, PromptCacheConfig};
use driver::{AgentDriver, AgentOrigin, BranchSpawner};
use handle::request_command;
use spawn::{build_agent, spawn_agent_driver, validate};
use turn::{Command, QueuedTurn, TurnKey};
