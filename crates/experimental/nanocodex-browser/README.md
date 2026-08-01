# Nanocodex Browser

Deterministic browser automation exposed as a normal Nanocodex Code Mode tool,
with an optional isolated headed-browser VM lifecycle.

`nanocodex-browser` is an experimental, unpublished, library-first crate. It
owns the browser action protocol, Chromium DevTools controller, diagnostics,
artifacts, and ordinary `BrowserTool`. Its named `vm` module composes those
browser concerns with `nanocodex-vm`; it does not own a second VM runtime.

The primary consumer is a Nanocodex agent. `BrowserTool` is caller-owned and is
not installed by default:

```rust,ignore
use nanocodex::{Nanocodex, OpenAi, Tools};
use nanocodex_browser::BrowserTool;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let browser = BrowserTool::new()?;
let tools = Tools::builder().provider(browser).build()?;
let openai = OpenAi::new(std::env::var("OPENAI_API_KEY")?)?;
let (agent, _events) = Nanocodex::builder(openai).tools(tools).build()?;

let result = agent
    .prompt("Open https://example.com, inspect it, and report the main heading.")
    .await?
    .result()
    .await?;
println!("{}", result.final_message());
# Ok(())
# }
```

The model reaches the tool as `await tools.browser(...)` inside Code Mode. Its
full contract is discoverable at runtime through `ALL_TOOLS`, but provider
registration keeps that contract out of the model-facing tool prefix. A
runnable version lives at `examples/browser_agent.rs`. Callers that deliberately
want the eager schema can register the same value with `.tool(browser)`.

For isolation, one non-cloneable VM owner keeps the disposable browser alive
and gives the agent a tool handle:

```no_run
use nanocodex_browser::{BrowserAction, vm::BrowserVm};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let browser = BrowserVm::builder(
    ".cache/browser/rootfs.ext4",
    "target/debug/nanocodex",
    ".cache/bin/gvproxy",
)
.vmm_args(["vm-run-config", "--config"])
.spawn()
.await?;

browser
    .browser()
    .execute(BrowserAction::Open {
        url: "https://example.com/".to_owned(),
    })
    .await?;

let tool = browser.tool();
// Pass `tool` to `Tools::builder().provider(tool)`.
drop(tool);
browser.shutdown().await?;
# Ok(())
# }
```

Every VM spawn reflinks an immutable ext4 template, runs Chromium as an
unprivileged guest user under Xvfb, and exposes CDP only through a random host
loopback port. The owner shuts down the DevTools controller, Chromium, gvproxy,
the VMM, and the disposable disk together. The image definition and guest init
script live in `image/`.

For trusted local development, `Browser` provides the same typed actions
without a VM:

```no_run
use nanocodex_browser::{Browser, BrowserAction};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let browser = Browser::new()?;
browser
    .execute(BrowserAction::Open {
        url: "https://example.com/".to_owned(),
    })
    .await?;
browser.close().await?;
# Ok(())
# }
```

The browser starts lazily on its first local action. Use `Browser::builder()`
for deterministic browser context, egress policy, storage state, diagnostics,
or an explicitly managed CDP endpoint.

## Capabilities

- Chromium navigation and interaction through semantic references, CSS, roles,
  text, labels, frames, tabs, and open shadow roots.
- Bounded snapshots, DOM/layout/style inspection, console and source-mapped
  errors, network bodies, WebSocket messages, HAR, and React diagnostics.
- Screenshots, PDF, visual/session/performance traces, video, CPU profiles,
  coverage, heap inspection, accessibility/axe, Lighthouse, and CrUX actions.
- Harness-owned cookies/storage, virtual passkeys, allowlisted Brave handoff,
  upload roots, browser egress policy, remote CDP, and libkrun VM composition.

The Nanocodex CLI can copy the standard Brave profile's complete cookie
database into either supported session-private browser:

```console
nanocodex --browser --cookies=true
nanocodex --browser=brave --cookies=true
```

The source profile remains open, is never mutated, and cookie values remain
outside the model-callable browser schema. This mode deliberately gives the
agent authenticated access to every site represented in the selected profile.

## Current boundaries

- This package is unpublished and its API is not stable. It currently targets
  native Unix hosts and Chromium's DevTools protocol; it is not a Firefox,
  Safari, WebDriver, WASM, Node, or Python browser adapter.
- Local mode is a private browser profile, not an OS sandbox. VM mode requires a
  prepared ext4 image, VMM entry point, gvproxy, and libkrun firmware. Its
  default network lease permits internet access; callers must supply their
  egress lease and browser policy when stronger restrictions are required.
- All browser actions remain available together. Their roughly 67 KiB contract
  is runtime-only with provider registration, so enabling browser adds no model
  warmup schema bytes. The contract is not capability-filtered after discovery.
- Lighthouse, CrUX, and video require caller-supplied external tooling or
  credentials. Brave profile transfer remains harness-owned and intentionally
  absent from the model-callable schema.
- Cloned browser handles share one session and serialized action stream. Call
  `Browser::close` or `BrowserVm::shutdown` when deterministic cleanup matters.
- The crate consumes `nanocodex-vm` unconditionally today, so local-only builds
  still pay the VM dependency's compile/link cost.
