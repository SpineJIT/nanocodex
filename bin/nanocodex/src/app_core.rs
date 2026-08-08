use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use eyre::{Result, WrapErr};
use nanocodex::{AgentEvents, Model, Thinking, agent::rollout::DurableSession};
use tokio::sync::mpsc;

use crate::{
    config::{AgentArgs, ConfiguredAgent},
    tui,
    vm::VmArgs,
};

pub use crate::tui::{WorkerCommand, WorkerEvent};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerCapability {
    Prompt,
    Steer,
    Cancel,
    Btw,
    Resume,
    HistoricalEdit,
    MainBranchSwitch,
}

impl WorkerCapability {
    const fn bit(self) -> u8 {
        match self {
            Self::Prompt => 1 << 0,
            Self::Steer => 1 << 1,
            Self::Cancel => 1 << 2,
            Self::Btw => 1 << 3,
            Self::Resume => 1 << 4,
            Self::HistoricalEdit => 1 << 5,
            Self::MainBranchSwitch => 1 << 6,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerCapabilities(u8);

impl WorkerCapabilities {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn with(self, capability: WorkerCapability) -> Self {
        Self(self.0 | capability.bit())
    }

    pub const fn standard() -> Self {
        Self::empty()
            .with(WorkerCapability::Prompt)
            .with(WorkerCapability::Steer)
            .with(WorkerCapability::Cancel)
            .with(WorkerCapability::Btw)
            .with(WorkerCapability::Resume)
            .with(WorkerCapability::HistoricalEdit)
            .with(WorkerCapability::MainBranchSwitch)
    }

    pub const fn supports(self, capability: WorkerCapability) -> bool {
        self.0 & capability.bit() != 0
    }
}

/// Starts one application worker and returns its command, event, and shutdown
/// handles without exposing the worker's agent implementation.
pub trait WorkerFactory {
    type Command;
    type Event;

    fn start(self) -> WorkerHandle<Self::Command, Self::Event>;
}

type ShutdownFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub struct WorkerHandle<Command, Event> {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<Event>,
    session_id: Arc<str>,
    capabilities: WorkerCapabilities,
    cleanup: ShutdownFuture,
}

impl<Command, Event> WorkerHandle<Command, Event> {
    pub fn new(
        commands: mpsc::UnboundedSender<Command>,
        events: mpsc::UnboundedReceiver<Event>,
        session_id: Arc<str>,
        capabilities: WorkerCapabilities,
        cleanup: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Self {
        Self {
            commands,
            events,
            session_id,
            capabilities,
            cleanup: Box::pin(cleanup),
        }
    }

    pub const fn capabilities(&self) -> WorkerCapabilities {
        self.capabilities
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub const fn commands(&self) -> &mpsc::UnboundedSender<Command> {
        &self.commands
    }

    pub const fn events_mut(&mut self) -> &mut mpsc::UnboundedReceiver<Event> {
        &mut self.events
    }

    pub async fn shutdown(self) -> Result<()> {
        let Self {
            commands,
            events,
            cleanup,
            ..
        } = self;
        drop((commands, events));
        cleanup.await
    }
}

/// Runs the shared interactive application against a caller-provided worker.
///
/// This application-layer entry point is intentionally separate from the public
/// `nanocodex` SDK. Alternate binaries can supply a different worker while
/// reusing the existing TUI, input handling, and terminal lifecycle.
pub async fn run_tui<F>(
    factory: F,
    cwd: PathBuf,
    model: Model,
    thinking: Thinking,
    fast_mode: bool,
    initial_prompt: Option<String>,
) -> Result<()>
where
    F: WorkerFactory<Command = WorkerCommand, Event = WorkerEvent>,
{
    tui::run_with_worker(
        factory,
        cwd,
        model,
        thinking,
        fast_mode,
        Vec::new(),
        initial_prompt.map(tui::InitialPrompt::plain),
    )
    .await
}

pub(crate) struct StandardWorkerFactory {
    configured: ConfiguredAgent,
}

impl StandardWorkerFactory {
    pub(crate) async fn build(config: AgentArgs, vm: VmArgs) -> Result<Self> {
        Ok(Self {
            configured: config.build(vm).await?,
        })
    }

    pub(crate) async fn build_resumed(
        config: AgentArgs,
        session: DurableSession,
        vm: VmArgs,
    ) -> Result<Self> {
        Ok(Self {
            configured: config.build_resumed(session, vm).await?,
        })
    }
}

impl WorkerFactory for StandardWorkerFactory {
    type Command = WorkerCommand;
    type Event = WorkerEvent;

    fn start(self) -> WorkerHandle<Self::Command, Self::Event> {
        let ConfiguredAgent {
            handle,
            events,
            realtime,
            child_agents,
            mpp_adapter,
            mcp,
            browser,
            vm,
        } = self.configured;
        let root_session_id = Arc::<str>::from(events.request_id());
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (updates, update_rx) = mpsc::unbounded_channel();
        let worker = tui::spawn_agent_worker(
            handle.clone(),
            Arc::clone(&root_session_id),
            realtime,
            mcp,
            command_rx,
            updates.clone(),
        );
        let root_events = forward_root_events(events, updates);

        WorkerHandle::new(
            commands,
            update_rx,
            root_session_id,
            WorkerCapabilities::standard(),
            async move {
                worker.abort();
                let worker_result = worker.await;
                root_events.abort();
                let root_events_result = root_events.await;
                let agent_shutdown_result = handle.shutdown().await;
                if let Some(child_agents) = child_agents {
                    child_agents.shutdown().await;
                }
                let browser_shutdown_result = if let Some(browser) = browser {
                    browser.shutdown().await
                } else {
                    Ok(())
                };
                let vm_shutdown_result = if let Some(vm) = vm {
                    vm.shutdown().await
                } else {
                    Ok(())
                };
                let mpp_shutdown_result = if let Some(adapter) = mpp_adapter {
                    adapter.shutdown().await
                } else {
                    Ok(())
                };

                match worker_result {
                    Ok(()) => {}
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => return Err(error).wrap_err("application worker failed"),
                }
                match root_events_result {
                    Ok(()) => {}
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => return Err(error).wrap_err("root event worker failed"),
                }
                agent_shutdown_result?;
                browser_shutdown_result?;
                vm_shutdown_result?;
                mpp_shutdown_result
            },
        )
    }
}

fn forward_root_events(
    mut events: AgentEvents,
    updates: mpsc::UnboundedSender<WorkerEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = events.recv_timed().await {
            if updates.send(WorkerEvent::RootAgentEvent { event }).is_err() {
                return;
            }
        }
        let _ = updates.send(WorkerEvent::RootEventStreamClosed);
    })
}

#[cfg(test)]
mod tests {
    use super::{WorkerCapabilities, WorkerCapability, WorkerHandle};
    use eyre::Result;
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot};

    #[test]
    fn standard_capabilities_expose_interactive_controls() {
        let capabilities = WorkerCapabilities::standard();

        assert!(capabilities.supports(WorkerCapability::Prompt));
        assert!(capabilities.supports(WorkerCapability::Steer));
        assert!(capabilities.supports(WorkerCapability::Cancel));
        assert!(capabilities.supports(WorkerCapability::Btw));
        assert!(capabilities.supports(WorkerCapability::Resume));
        assert!(capabilities.supports(WorkerCapability::HistoricalEdit));
        assert!(capabilities.supports(WorkerCapability::MainBranchSwitch));
    }

    #[test]
    fn capabilities_describe_an_alternate_workers_supported_controls() {
        let capabilities = WorkerCapabilities::empty().with(WorkerCapability::Prompt);

        assert!(capabilities.supports(WorkerCapability::Prompt));
        assert!(!capabilities.supports(WorkerCapability::Btw));
    }

    #[tokio::test]
    async fn shutdown_closes_commands_before_awaiting_cleanup() -> Result<()> {
        let (commands, mut command_rx) = mpsc::unbounded_channel::<()>();
        let (_updates, update_rx) = mpsc::unbounded_channel::<()>();
        let (cleaned_tx, cleaned_rx) = oneshot::channel();
        let worker = WorkerHandle::new(
            commands,
            update_rx,
            Arc::from("test-session"),
            WorkerCapabilities::standard(),
            async move {
                assert!(command_rx.recv().await.is_none());
                cleaned_tx.send(()).unwrap();
                Ok(())
            },
        );

        worker.shutdown().await?;
        cleaned_rx.await.unwrap();
        Ok(())
    }
}
