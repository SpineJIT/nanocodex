use std::{env, error::Error, path::PathBuf};

use nanocodex::oai::auth::OpenAiAuth;
use nanocodex_eval::{Task, VmResources, vm::VmBackend};

pub type AnyError = Box<dyn Error + Send + Sync>;

pub fn auth() -> Result<OpenAiAuth, AnyError> {
    if let Ok(api_key) = env::var("OPENAI_API_KEY")
        && !api_key.trim().is_empty()
    {
        return Ok(OpenAiAuth::api_key(api_key));
    }
    let path = env::var_os("NANOCODEX_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| env::var_os("CODEX_HOME").map(|root| PathBuf::from(root).join("auth.json")))
        .ok_or("set OPENAI_API_KEY or NANOCODEX_AUTH_FILE")?;
    Ok(nanocodex::oai::auth::load_chatgpt_auth(path)?)
}

pub async fn vm_backend(tasks: Vec<Task>) -> Result<VmBackend, AnyError> {
    Ok(vm_resources(tasks).await?.backend().await?)
}

pub async fn vm_resources(tasks: Vec<Task>) -> Result<VmResources, AnyError> {
    let vmm = env::var_os("NANOCODEX_BIN")
        .map_or_else(|| PathBuf::from("target/debug/nanocodex"), PathBuf::from);
    let runtime = env::var_os("NANOCODEX_VM_RUNTIME")
        .map_or_else(|| PathBuf::from(".cache/vm/runtime.ext4"), PathBuf::from);
    Ok(VmResources::builder(vmm, runtime)
        .tasks(tasks)
        .prepare()
        .await?)
}
