use std::{net::IpAddr, path::PathBuf};

use clap::Args;
use eyre::{Result, WrapErr as _};
use nanocodex_eval::{Evaluation, coordinator::CoordinatorServer};
use tokio::net::TcpListener;

use super::profile::default_state_dir;

#[derive(Args)]
pub(super) struct Coordinator {
    /// Evaluation profile. Uses the manifest's top-level default when omitted.
    profile: Option<String>,

    /// Evaluation manifest containing the closed desired work bundle.
    #[arg(long, default_value = "nanocodex.toml")]
    config: PathBuf,

    /// Durable SQLite ledger and retained coordinator artifacts.
    #[arg(long, value_name = "DIRECTORY")]
    state_dir: Option<PathBuf>,

    /// Loopback or Tailscale address to listen on.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,

    /// Listen port. Use zero to allocate an available port.
    #[arg(long, default_value_t = 8789)]
    port: u16,
}

impl Coordinator {
    pub(super) async fn run(self) -> Result<()> {
        let state = self.state_dir.map_or_else(default_state_dir, Ok)?;
        let evaluation = Evaluation::open(&self.config, self.profile.as_deref(), state)?;
        let listener = TcpListener::bind((self.bind, self.port))
            .await
            .wrap_err("failed to bind the evaluation coordinator")?;
        let address = listener.local_addr()?;
        println!("http://{address}");
        CoordinatorServer::new(evaluation)
            .serve(listener)
            .await
            .wrap_err("evaluation coordinator stopped")
    }
}
