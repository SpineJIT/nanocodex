#[cfg(not(target_family = "wasm"))]
mod agents_md;

#[cfg(not(target_family = "wasm"))]
#[path = "native.rs"]
mod platform;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[path = "web.rs"]
mod platform;

pub(crate) use platform::ContextSource;
pub(super) use platform::ContextSourceConfig;
