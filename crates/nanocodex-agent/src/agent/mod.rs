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
    __private::{EventSink, ModelConfig, ResponsesServiceFactory, into_openai_parts},
    Model, OpenAi, Prompt, ReasoningMode, ResponseError, Thinking,
    auth::OpenAiAuthMode,
    events::{AgentEvent, AgentEvents},
    session::SessionId,
    tower::{ResponsesAttempt, ResponsesClient, ResponsesServiceResponse, StandardServiceFactory},
    transport::{ResponsesHistory, ResponsesTransport, TransportStats},
};
use nanocodex_tools::Tools;
#[cfg(not(target_family = "wasm"))]
use nanocodex_tools::ToolsBuildError;
use tokio::sync::{mpsc, oneshot, watch};
use tower::Service;
use tracing::{Instrument, info, info_span};

use crate::prompt_cache::{ModelPromptCache, SharedPromptCache};
use crate::{
    NanocodexError, Result,
    model::run::{
        CompletedModelTurn, HistoryCheckpoint, ModelCheckpoint, ModelCompactOutcome, ModelRun,
        ModelTurnOutcome, PreparedCheckpoint, prepare_history_checkpoint,
        prepare_resumed_checkpoint,
    },
    session::{CommittedSession, SessionResume, SessionSnapshot},
    usage::TurnUsage,
};

const COMMAND_CAPACITY: usize = 8;
const STEER_CAPACITY: usize = 8;

#[cfg(not(target_family = "wasm"))]
type ToolsFactory =
    Arc<dyn Fn(AgentHandle) -> std::result::Result<Tools, ToolsBuildError> + Send + Sync>;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ToolProfile {
    Primary,
    Child,
}

enum InitialResume {
    Exact(Box<ModelCheckpoint>),
    History(Box<HistoryCheckpoint>),
}

impl InitialResume {
    fn workspace(&self) -> &str {
        match self {
            Self::Exact(checkpoint) => checkpoint.workspace(),
            Self::History(resume) => &resume.workspace,
        }
    }

    fn history_len(&self) -> usize {
        match self {
            Self::Exact(checkpoint) => checkpoint.history().len(),
            Self::History(resume) => resume.history.len(),
        }
    }

    fn prompt_cache_key(&self) -> &str {
        match self {
            Self::Exact(checkpoint) => checkpoint.prompt_cache_key(),
            Self::History(resume) => &resume.prompt_cache_key,
        }
    }
}

#[derive(Clone)]
enum ToolsConfiguration {
    Shared(Tools),
    #[cfg(not(target_family = "wasm"))]
    PerAgent {
        factory: ToolsFactory,
        child_factory: Option<ToolsFactory>,
    },
}

impl ToolsConfiguration {
    #[cfg(not(target_family = "wasm"))]
    pub(super) const fn has_child_factory(&self) -> bool {
        matches!(
            self,
            Self::PerAgent {
                child_factory: Some(_),
                ..
            }
        )
    }

    #[cfg(target_family = "wasm")]
    pub(super) const fn has_child_factory(&self) -> bool {
        false
    }

    fn materialize(&self, agent_handle: AgentHandle, profile: ToolProfile) -> Result<Tools> {
        #[cfg(all(target_family = "wasm", target_os = "unknown"))]
        let _ = (agent_handle, profile);
        match self {
            Self::Shared(tools) => Ok(tools.clone()),
            #[cfg(not(target_family = "wasm"))]
            Self::PerAgent {
                factory,
                child_factory,
            } => child_factory
                .as_ref()
                .filter(|_| matches!(profile, ToolProfile::Child))
                .unwrap_or(factory)(agent_handle)
            .map_err(Into::into),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn with_child_factory(self, child_factory: ToolsFactory) -> Self {
        match self {
            Self::Shared(tools) => {
                let factory: ToolsFactory = Arc::new(move |_agent| Ok(tools.clone()));
                Self::PerAgent {
                    factory,
                    child_factory: Some(child_factory),
                }
            }
            Self::PerAgent { factory, .. } => Self::PerAgent {
                factory,
                child_factory: Some(child_factory),
            },
        }
    }
}

mod builder;
mod context_source;
mod driver;
mod durability;
mod executor;
mod handle;
mod session_context;
mod spawn;
mod turn;

pub use builder::NanocodexBuilder;
pub use context_source::ExecutionEnvironment;
pub use handle::{AgentHandle, Nanocodex};
pub use session_context::AgentSessionContext;
pub use turn::{PromptRoute, Turn, TurnCompletion, TurnControl, TurnResult};

use builder::{CodexCompatibility, PromptCacheConfig};
pub(crate) use context_source::ContextSource;
use context_source::ContextSourceConfig;
use driver::{AgentDriver, AgentOrigin, BranchSpawner, DriverFailure, DriverShutdown};
use durability::{Durability, DurabilityConfig};
pub(crate) use executor::{AgentFactory, AgentSend};
use executor::{ServiceFactory, spawn_driver};
use handle::request_command;
use spawn::{build_agent, spawn_agent_driver, validate};
use turn::{Command, PromptRouteKind, QueuedTurn, TurnKey};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
