use std::sync::Arc;

use eyre::Result;
use nanocodex_bin::app_core::{
    WorkerCapabilities, WorkerCapability, WorkerCommand, WorkerEvent, WorkerFactory, WorkerHandle,
};
use tokio::sync::mpsc;

struct TestWorkerFactory;

impl WorkerFactory for TestWorkerFactory {
    type Command = WorkerCommand;
    type Event = WorkerEvent;

    fn start(self) -> WorkerHandle<Self::Command, Self::Event> {
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let (_updates, events) = mpsc::unbounded_channel();
        WorkerHandle::new(
            commands,
            events,
            Arc::from("test-session"),
            WorkerCapabilities::standard(),
            async { Ok(()) },
        )
    }
}

#[tokio::test]
async fn external_worker_factory_uses_the_app_core_contract() -> Result<()> {
    let worker = TestWorkerFactory.start();

    assert!(worker.capabilities().supports(WorkerCapability::Prompt));
    worker.shutdown().await
}
