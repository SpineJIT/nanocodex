use std::{path::PathBuf, process::Command, time::Duration};

use clap::{Args, Subcommand};
use eyre::{Result, eyre};
use nanousd::{
    CreateOrderRequest, CreateOrderResponse, CreditsClient, DEFAULT_API_URL, NANOUSD_DECIMALS,
    Order, OrderStatus,
};
use tempo_alloy::accounts::TempoAccountsStore;

#[derive(Args)]
pub(crate) struct Credits {
    #[command(subcommand)]
    command: CreditsCommand,

    /// `NanoUSD` credits API endpoint.
    #[arg(
        long,
        global = true,
        env = "NANOCODEX_CREDITS_API_URL",
        default_value = DEFAULT_API_URL
    )]
    api_url: String,

    /// Tempo Wallet state containing the destination account.
    #[arg(long, global = true, env = "NANOCODEX_PROVIDER_TEMPO_WALLET_STORE")]
    wallet_store: Option<PathBuf>,
}

#[derive(Subcommand)]
enum CreditsCommand {
    /// Show the current wallet's `NanoUSD` balance and service configuration.
    Status(OutputArgs),
    /// Purchase a fixed-dollar package of Nanocodex credits.
    Buy(BuyArgs),
    /// Wait for an existing order to be fulfilled.
    Wait(WaitArgs),
}

#[derive(Args)]
struct OutputArgs {
    /// Emit the response as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct BuyArgs {
    /// Package value in whole US dollars, for example `10`, `25`, or `50`.
    dollars: u64,

    /// Print the checkout URL instead of opening it.
    #[arg(long)]
    no_open: bool,

    /// Return as soon as the order is created.
    #[arg(long)]
    no_wait: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Maximum time to wait for payment and issuance.
    #[arg(long, default_value_t = 600)]
    timeout_seconds: u64,
}

#[derive(Args)]
struct WaitArgs {
    order_id: String,

    /// Order capability returned by `credits buy --json --no-wait`.
    #[arg(long, env = "NANOCODEX_CREDITS_ORDER_TOKEN", hide_env_values = true)]
    order_token: String,

    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Maximum time to wait for fulfillment.
    #[arg(long, default_value_t = 600)]
    timeout_seconds: u64,
}

impl Credits {
    pub(crate) async fn run(self) -> Result<()> {
        let store = self
            .wallet_store
            .map_or_else(TempoAccountsStore::open_default, TempoAccountsStore::open)?;
        let account = store.active_account()?;
        let client = CreditsClient::new(&self.api_url)?;
        match self.command {
            CreditsCommand::Status(args) => status(&client, account, args.json).await,
            CreditsCommand::Buy(args) => buy(&client, account, args).await,
            CreditsCommand::Wait(args) => {
                let order = wait_for_order(
                    &client,
                    &args.order_id,
                    &args.order_token,
                    Duration::from_secs(args.timeout_seconds),
                    !args.json,
                )
                .await?;
                print_order(&order, args.json)
            }
        }
    }
}

async fn status(
    client: &CreditsClient,
    wallet: alloy_primitives::Address,
    json: bool,
) -> Result<()> {
    let (info, balance) = tokio::try_join!(client.info(), client.balance(wallet))?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "service": info,
                "balance": balance,
            }))?
        );
    } else {
        println!("Nanocodex credits\n");
        println!("Available:  ${}", format_units(balance.nanousd_units));
        println!("Currency:   NANOUSD");
        println!("Wallet:     {}", balance.wallet);
        println!("Network:    Tempo mainnet ({})", balance.chain_id);
        println!("Token:      {}", balance.token);
        println!("Onramp:     {}", info.payment_mode);
        let packages = info
            .packages
            .iter()
            .map(|package| format!("${}", package.usd_cents / 100))
            .collect::<Vec<_>>()
            .join(", ");
        println!("Packages:   {packages}");
    }
    Ok(())
}

async fn buy(
    client: &CreditsClient,
    wallet: alloy_primitives::Address,
    args: BuyArgs,
) -> Result<()> {
    let package_cents = args
        .dollars
        .checked_mul(100)
        .ok_or_else(|| eyre!("credit package is too large"))?;
    let created = client
        .create_order(&CreateOrderRequest {
            wallet,
            package_cents,
        })
        .await?;
    if args.json && args.no_wait {
        println!("{}", serde_json::to_string(&created)?);
        return Ok(());
    }
    describe_created(&created, args.json);
    if let Some(checkout_url) = &created.order.checkout_url {
        if args.no_open {
            if !args.json {
                println!("Checkout: {checkout_url}");
            }
        } else if let Err(error) = open_browser(checkout_url) {
            if !args.json {
                eprintln!(
                    "Could not open checkout automatically ({error}). Open this URL:\n{checkout_url}"
                );
            }
        } else if !args.json {
            println!("Opened secure Stripe Checkout in your browser.");
        }
    }
    if args.no_wait {
        if !args.json {
            println!(
                "Resume with: nanocodex credits wait {} --order-token <token>",
                created.order.id
            );
            println!("Order token: {}", created.order_token);
        }
        return Ok(());
    }
    let order = wait_for_order(
        client,
        &created.order.id,
        &created.order_token,
        Duration::from_secs(args.timeout_seconds),
        !args.json,
    )
    .await?;
    print_order(&order, args.json)
}

fn describe_created(created: &CreateOrderResponse, json: bool) {
    if !json {
        println!(
            "Order {} created for ${} NANOUSD → {}",
            created.order.id,
            format_units(created.order.package.nanousd_units),
            created.order.wallet
        );
        if created.order.checkout_url.is_none() {
            println!("Mock payment accepted; waiting for Tempo issuance…");
        }
    }
}

async fn wait_for_order(
    client: &CreditsClient,
    id: &str,
    token: &str,
    timeout: Duration,
    progress: bool,
) -> Result<Order> {
    let started = tokio::time::Instant::now();
    let mut previous = None;
    loop {
        let order = client.order(id, token).await?;
        if previous != Some(order.status) && progress {
            println!("Order status: {}", status_label(order.status));
        }
        previous = Some(order.status);
        if order.status == OrderStatus::Fulfilled {
            return Ok(order);
        }
        if order.status == OrderStatus::Expired {
            return Err(eyre!("NanoUSD order {id} expired before payment"));
        }
        if started.elapsed() >= timeout {
            return Err(eyre!(
                "timed out waiting for NanoUSD order {id}; it remains safe to resume with `nanocodex credits wait`"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn print_order(order: &Order, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(order)?);
    } else {
        println!(
            "✓ ${} NANOUSD issued to {}",
            format_units(order.package.nanousd_units),
            order.wallet
        );
        if let Some(transaction_hash) = &order.transaction_hash {
            println!("Tempo transaction: {transaction_hash}");
        }
    }
    Ok(())
}

fn format_units(units: u64) -> String {
    let scale = 10_u64.pow(NANOUSD_DECIMALS);
    format!("{}.{:02}", units / scale, (units % scale) / 10_000)
}

fn status_label(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::Created => "created",
        OrderStatus::AwaitingPayment => "waiting for payment",
        OrderStatus::Paid => "paid",
        OrderStatus::Fulfilling => "issuing NANOUSD",
        OrderStatus::Fulfilled => "fulfilled",
        OrderStatus::Failed => "issuance retry scheduled",
        OrderStatus::Expired => "expired",
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "automatic browser launch is unsupported on this platform",
    ));
    command.arg(url);
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "browser launcher exited with {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        credits: Credits,
    }

    #[test]
    fn credits_api_url_is_explicitly_configurable() {
        let cli = TestCli::try_parse_from([
            "nanocodex-credits",
            "--api-url",
            "https://credits.example.test",
            "status",
        ])
        .unwrap();

        assert_eq!(cli.credits.api_url, "https://credits.example.test");
    }

    #[test]
    fn credits_api_url_defaults_to_loopback() {
        let cli = TestCli::try_parse_from(["nanocodex-credits", "status"]).unwrap();

        assert_eq!(cli.credits.api_url, DEFAULT_API_URL);
        assert!(cli.credits.api_url.starts_with("http://127.0.0.1:"));
    }
}
