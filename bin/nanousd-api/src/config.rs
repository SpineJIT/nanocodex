use std::{net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};

use alloy_primitives::Address;
use clap::{Parser, ValueEnum};
use eyre::{Result, eyre};
use nanousd::{DEFAULT_API_URL, NANOUSD_ADDRESS};

const DEFAULT_RPC_URL: &str = "https://rpc.mainnet.tempo.xyz";
// The issuer account is funded in PathUSD. Deployments may override this to
// match their dedicated issuer account and access-key policy.
const DEFAULT_FEE_TOKEN: &str = "0x20c0000000000000000000000000000000000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum PaymentMode {
    Mock,
    Stripe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum IssuerMode {
    Mock,
    Alloy,
}

#[derive(Debug, Parser)]
#[command(about = "NanoUSD credits onramp and TIP-1015 fulfillment service")]
pub(crate) struct Config {
    #[arg(long, env = "NANOUSD_BIND", default_value = "127.0.0.1:8789")]
    pub bind: SocketAddr,

    #[arg(long, env = "NANOUSD_PUBLIC_URL", default_value = DEFAULT_API_URL)]
    pub public_url: String,

    #[arg(
        long,
        env = "NANOUSD_DATABASE",
        default_value = ".nanocodex/nanousd-api.sqlite3"
    )]
    pub database: PathBuf,

    #[arg(long, env = "NANOUSD_PAYMENT_MODE", value_enum, default_value = "mock")]
    pub payment_mode: PaymentMode,

    #[arg(long, env = "NANOUSD_ISSUER_MODE", value_enum, default_value = "mock")]
    pub issuer_mode: IssuerMode,

    #[arg(long, env = "NANOUSD_TOKEN", default_value_t = NANOUSD_ADDRESS)]
    pub token: Address,

    #[arg(long, env = "NANOUSD_RPC_URL", default_value = DEFAULT_RPC_URL)]
    pub rpc_url: String,

    #[arg(long, env = "NANOUSD_FEE_TOKEN", default_value = DEFAULT_FEE_TOKEN)]
    pub fee_token: String,

    #[arg(long, env = "NANOUSD_WALLET_STORE")]
    pub wallet_store: Option<PathBuf>,

    #[arg(long, env = "NANOUSD_STRIPE_SECRET_KEY", hide_env_values = true)]
    pub stripe_secret_key: Option<String>,

    #[arg(long, env = "NANOUSD_STRIPE_WEBHOOK_SECRET", hide_env_values = true)]
    pub stripe_webhook_secret: Option<String>,

    /// Optional Stripe API version. Omit it to use the account's configured version.
    #[arg(long, env = "NANOUSD_STRIPE_API_VERSION")]
    pub stripe_api_version: Option<String>,

    #[arg(
        long,
        env = "NANOUSD_STRIPE_API_URL",
        default_value = "https://api.stripe.com"
    )]
    pub stripe_api_url: String,

    #[arg(long, env = "NANOUSD_WEBHOOK_TOLERANCE_SECONDS", default_value_t = 300)]
    pub webhook_tolerance_seconds: u64,

    #[arg(long, env = "NANOUSD_FULFILLMENT_POLL_MS", default_value_t = 500)]
    pub fulfillment_poll_ms: u64,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        let public = reqwest::Url::parse(&self.public_url)
            .map_err(|error| eyre!("invalid NANOUSD_PUBLIC_URL: {error}"))?;
        if self.payment_mode == PaymentMode::Mock && !self.bind.ip().is_loopback() {
            return Err(eyre!(
                "mock payment mode is a faucet and may only bind to a loopback address"
            ));
        }
        if self.payment_mode == PaymentMode::Stripe {
            if public.scheme() != "https" {
                return Err(eyre!("Stripe mode requires an HTTPS public URL"));
            }
            if self.stripe_secret_key.is_none() || self.stripe_webhook_secret.is_none() {
                return Err(eyre!(
                    "Stripe mode requires NANOUSD_STRIPE_SECRET_KEY and NANOUSD_STRIPE_WEBHOOK_SECRET"
                ));
            }
        }
        let _ = Address::from_str(&self.fee_token)
            .map_err(|error| eyre!("invalid NANOUSD_FEE_TOKEN: {error}"))?;
        Ok(())
    }

    pub const fn worker_interval(&self) -> Duration {
        Duration::from_millis(self.fulfillment_poll_ms)
    }
}
