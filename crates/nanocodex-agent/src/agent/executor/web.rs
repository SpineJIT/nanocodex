use std::{future::Future, sync::Arc};

use crate::Result;

pub(crate) type ServiceFactory<S> = Arc<dyn Fn() -> S>;

pub(crate) trait AgentSend {}

impl<T> AgentSend for T {}

pub(crate) trait AgentFactory {}

impl<T> AgentFactory for T {}

pub(crate) fn spawn_driver<F>(driver: F) -> Result<()>
where
    F: Future<Output = ()> + 'static,
{
    wasm_bindgen_futures::spawn_local(driver);
    Ok(())
}
