use std::{
    collections::HashMap,
    future::{Pending, pending},
    sync::{Arc, Mutex},
    time::Instant,
};

use nanocodex_agent::{
    Nanocodex, NanocodexError, Responses, ResponsesAttempt, ResponsesServiceResponse, Tools,
};
use tokio::sync::mpsc;
use tower::Service;
use tracing::{Id, Instrument, Subscriber, info_span, span::Attributes};
use tracing_subscriber::{Layer, layer::Context as LayerContext, prelude::*, registry::LookupSpan};

#[derive(Clone)]
struct PendingService;

impl Service<ResponsesAttempt> for PendingService {
    type Response = ResponsesServiceResponse;
    type Error = NanocodexError;
    type Future = Pending<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
        pending()
    }
}

#[derive(Clone, Default)]
struct TraceCapture(Arc<Mutex<HashMap<u64, CapturedSpan>>>);

#[derive(Clone)]
struct CapturedSpan {
    name: &'static str,
    parent: Option<u64>,
    opened: Instant,
    closed: Option<Instant>,
}

impl<S> Layer<S> for TraceCapture
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: LayerContext<'_, S>) {
        let parent = attributes
            .parent()
            .map(|parent| parent.clone().into_u64())
            .or_else(|| {
                attributes
                    .is_contextual()
                    .then(|| context.current_span().id().map(Id::into_u64))
                    .flatten()
            });
        self.0.lock().unwrap().insert(
            id.clone().into_u64(),
            CapturedSpan {
                name: attributes.metadata().name(),
                parent,
                opened: Instant::now(),
                closed: None,
            },
        );
    }

    fn on_close(&self, id: Id, _context: LayerContext<'_, S>) {
        if let Some(span) = self.0.lock().unwrap().get_mut(&id.into_u64()) {
            span.closed = Some(Instant::now());
        }
    }
}

#[test]
fn contextual_child_turns_preserve_parallel_orchestration_parentage() {
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let dispatch = tracing::Dispatch::new(subscriber);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    tracing::dispatcher::with_default(&dispatch, || {
        runtime.block_on(async {
            let (handles, mut received_handles) = mpsc::unbounded_channel();
            let responses = Responses::builder().service(|| PendingService).build();
            let (root, root_events) = Nanocodex::builder("test")
                .responses(responses)
                .tools_factory(move |handle| {
                    drop(handles.send(handle));
                    Tools::builder().without_defaults().build()
                })
                .build()
                .unwrap();
            let root_handle = received_handles.recv().await.unwrap();
            let (child_a, first_events) = root_handle.spawn().await.unwrap();
            let (child_b, second_events) = root_handle.spawn().await.unwrap();
            let (controls, mut received_controls) = mpsc::unbounded_channel();

            let (task_a, task_b) = async {
                let controls_a = controls.clone();
                let task_a = tokio::spawn(
                    async move {
                        let turn = child_a.prompt("child a").await.unwrap();
                        controls_a.send(turn.control()).unwrap();
                        assert!(matches!(
                            turn.result().await,
                            Err(NanocodexError::TurnCancelled)
                        ));
                    }
                    .instrument(info_span!("test.spawn_agent", child = "a")),
                );
                let task_b = tokio::spawn(
                    async move {
                        let turn = child_b.prompt("child b").await.unwrap();
                        controls.send(turn.control()).unwrap();
                        assert!(matches!(
                            turn.result().await,
                            Err(NanocodexError::TurnCancelled)
                        ));
                    }
                    .instrument(info_span!("test.spawn_agent", child = "b")),
                );
                (task_a, task_b)
            }
            .instrument(info_span!("test.code_mode.cell"))
            .await;

            let control_a = received_controls.recv().await.unwrap();
            let control_b = received_controls.recv().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let (cancel_a, cancel_b) = tokio::join!(control_a.cancel(), control_b.cancel());
            cancel_a.unwrap();
            cancel_b.unwrap();
            task_a.await.unwrap();
            task_b.await.unwrap();

            let responses = Responses::builder().service(|| PendingService).build();
            let (plain, plain_events) = Nanocodex::builder("test")
                .responses(responses)
                .build()
                .unwrap();
            let plain_turn = plain.prompt("plain root turn").await.unwrap();
            plain_turn.cancel().await.unwrap();
            assert!(matches!(
                plain_turn.result().await,
                Err(NanocodexError::TurnCancelled)
            ));

            drop((plain, plain_events, root, root_events));
            drop((first_events, second_events));
        });
    });

    let spans = capture.0.lock().unwrap();
    let turns = spans
        .iter()
        .filter(|(_, span)| span.name == "agent.turn")
        .collect::<Vec<_>>();
    assert_eq!(turns.len(), 3);

    let child_turns = turns
        .iter()
        .filter(|(_, span)| {
            span.parent
                .and_then(|parent| spans.get(&parent))
                .is_some_and(|parent| parent.name == "test.spawn_agent")
        })
        .map(|(_, span)| *span)
        .collect::<Vec<_>>();
    assert_eq!(child_turns.len(), 2);
    assert!(turns.iter().any(|(_, span)| span.parent.is_none()));

    let first = child_turns[0];
    let second = child_turns[1];
    assert!(
        first.opened < second.closed.unwrap() && second.opened < first.closed.unwrap(),
        "child turn intervals should overlap"
    );
}
