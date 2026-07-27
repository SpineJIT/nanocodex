//! Generic Tower attempt, service, retry, and streamed-output contracts.

pub(crate) mod attempt;
pub(crate) mod client;
pub(crate) mod middleware;
pub(crate) mod service;
pub(crate) mod service_error;
pub(crate) mod stream;

#[doc(inline)]
pub use crate::openai::StandardServiceFactory;
#[doc(hidden)]
pub use crate::openai::{CallerServiceFactory, LayeredServiceFactory, MakeResponsesService};
#[doc(inline)]
pub use attempt::{
    ResponsesAttempt, ResponsesAttemptFactory, ResponsesAttemptKind, ResponsesOutput,
    ResponsesServiceResponse,
};
#[doc(inline)]
pub use client::ResponsesClient;
#[doc(inline)]
pub use middleware::{DefaultResponsesService, ResponsesRetryPolicy};
#[doc(inline)]
pub use service::ResponsesService;
#[doc(inline)]
pub use service_error::ResponsesServiceError;
#[doc(inline)]
pub use stream::{
    CodeCall, CodeCallKind, CompactionOutput, GenerationOutput, ResponsePipelineStats,
};
