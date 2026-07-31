# nanocodex-egress

`nanocodex-egress` is Nanocodex's unpublished, experimental HTTP egress
transport. It extracts the authenticated loopback proxy that previously lived
inside the Tempo MPP adapter without changing the adapter's child-process
contract.

The crate owns proxy authentication, TLS interception, bounded replayable
request bodies, forwarding concurrency, and lifecycle. Application protocols
remain outside the crate and compose through `EgressLayer`; the Nanocodex
binary implements its Tempo payment layer using MPP's request middleware.

```rust,no_run
use nanocodex_egress::{
    EgressLayer, EgressProxy,
    middleware::{
        Extensions, Next, Request, Response, Result as MiddlewareResult,
        async_trait,
    },
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

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let proxy = EgressProxy::builder()
    .layer(ApplicationLayer)
    .spawn()
    .await?;

let child_environment = proxy.environment();
assert!(!child_environment.is_empty());
assert!(proxy.ca_certificate_path().is_file());
proxy.shutdown().await?;
# Ok(())
# }
```

Layers run in builder order. The proxy keeps its random authentication
credential and ephemeral CA in the host process, while `environment()`
returns the same curl, Requests, Node, and MPP compatibility variables that the
previous embedded MPP proxy exposed. Apply those variables only to tool child
processes, not to Nanocodex's control-plane process.
