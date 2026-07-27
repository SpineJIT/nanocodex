# Nanocodex Observability

Application-owned tracing and OpenTelemetry setup for Nanocodex.

The agent lifecycle emits complete structured spans and events through
`tracing`. This crate installs one process-wide subscriber that can write those
records locally, export them over OTLP/HTTP, or do both.

## Local tracing

Keep the returned guard alive for as long as spans may be emitted:

```rust
use nanocodex_observability::{LogFormat, ObservabilityBuilder};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let _observability = ObservabilityBuilder::new("checkout-agent", "1.4.0")
    .environment("production")
    .filter("warn,nanocodex=info,nanocodex_oai_api=info,nanocodex_tools=info")
    .format(LogFormat::Json)
    .install()?;
# Ok(())
# }
```

Local records go to stderr by default. Use
[`LogOutput::File`](nanocodex_observability::LogOutput::File) when the embedding
application owns a durable trace path.

## OTLP export

Set a collector base endpoint to add OTLP/HTTP export alongside local output:

```rust
use nanocodex_observability::ObservabilityBuilder;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut observability = ObservabilityBuilder::new("checkout-agent", "1.4.0")
    .environment("staging")
    .otlp_endpoint("http://127.0.0.1:4318")
    .install()?;

observability.shutdown()?;
# Ok(())
# }
```

[`ObservabilityGuard::shutdown`](nanocodex_observability::ObservabilityGuard::shutdown)
explicitly flushes pending spans. Dropping the guard also attempts shutdown,
but cannot report a flush error.
