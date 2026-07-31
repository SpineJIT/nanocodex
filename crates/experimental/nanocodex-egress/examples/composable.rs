use std::error::Error;

use nanocodex_egress::{
    EgressLayer, EgressProxy,
    middleware::{Extensions, Next, Request, Response, Result as MiddlewareResult, async_trait},
};

struct ApplicationLayer;

#[async_trait]
impl EgressLayer for ApplicationLayer {
    async fn handle(
        &self,
        request: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> MiddlewareResult<Response> {
        next.run(request, extensions).await
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<(), Box<dyn Error>> {
    let proxy = EgressProxy::builder()
        .layer(ApplicationLayer)
        .spawn()
        .await?;

    println!(
        "composed application egress with {} child variables",
        proxy.environment().len()
    );
    proxy.shutdown().await?;
    Ok(())
}
