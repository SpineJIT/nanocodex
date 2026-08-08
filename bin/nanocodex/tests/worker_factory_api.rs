use std::sync::Arc;

use eyre::Result;
use nanocodex_bin::app_core::{
    VoiceControl, WorkerCapabilities, WorkerCapability, WorkerCommand, WorkerEvent, WorkerFactory,
    WorkerHandle,
};
use tokio::sync::mpsc;

struct TestWorkerFactory;

const fn voice_command(control: VoiceControl) -> WorkerCommand {
    WorkerCommand::Voice(control)
}

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
    let _ = voice_command(VoiceControl::List);
    worker.shutdown().await
}
