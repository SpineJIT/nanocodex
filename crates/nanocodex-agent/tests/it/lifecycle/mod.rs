use std::{
    future::{Future, Pending, Ready, pending},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use nanocodex_agent::{
    Nanocodex, NanocodexError, OpenAi, Tools,
    rollout::RolloutConfig,
    session::SessionId,
    transport::{ResponsesAttempt, ResponsesServiceResponse},
};
use nanocodex_tools::{
    ToolContext, ToolDefinition, contract::ToolExecution, runtime::DynamicToolProvider,
};
use serde_json::Value;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tower::{Service, ServiceBuilder, limit::ConcurrencyLimitLayer, timeout::TimeoutLayer};

#[derive(Clone)]
struct NeverCalled;

impl Service<ResponsesAttempt> for NeverCalled {
    type Response = ResponsesServiceResponse;
    type Error = NanocodexError;
    type Future = Ready<std::result::Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
        panic!("the service is not called by this test")
    }
}

fn test_openai() -> OpenAi {
    OpenAi::new("test").unwrap()
}

#[derive(Clone)]
struct PendingService;

#[derive(Clone)]
struct DropPendingService {
    started: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

struct DropPendingFuture {
    dropped: Arc<AtomicBool>,
}

impl Future for DropPendingFuture {
    type Output = std::result::Result<ResponsesServiceResponse, NanocodexError>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl Drop for DropPendingFuture {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

impl Service<ResponsesAttempt> for DropPendingService {
    type Response = ResponsesServiceResponse;
    type Error = NanocodexError;
    type Future = DropPendingFuture;

    fn poll_ready(
        &mut self,
        _context: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
        self.started.store(true, Ordering::Release);
        DropPendingFuture {
            dropped: Arc::clone(&self.dropped),
        }
    }
}

struct StartProbe(Arc<AtomicBool>);

#[async_trait]
impl DynamicToolProvider for StartProbe {
    fn start(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn direct_tools(&self) -> Vec<Arc<dyn nanocodex_tools::Tool>> {
        Vec::new()
    }

    fn available_definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    async fn execute(
        &self,
        _name: &str,
        _input: Value,
        _context: ToolContext<'_>,
    ) -> Option<ToolExecution> {
        None
    }
}

impl Service<ResponsesAttempt> for PendingService {
    type Response = ResponsesServiceResponse;
    type Error = NanocodexError;
    type Future = Pending<std::result::Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
        pending()
    }
}

mod builder;
mod control;
