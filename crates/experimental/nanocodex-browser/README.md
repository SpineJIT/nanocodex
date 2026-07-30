# Nanocodex Browser

Deterministic browser automation and an isolated headed-browser lifecycle for
Nanocodex.

`nanocodex-browser` is an experimental, unpublished, library-first crate. It
owns the browser action protocol, Chromium DevTools controller, diagnostics,
artifacts, and ordinary `BrowserTool`. Its named `vm` module composes those
browser concerns with `nanocodex-vm`; it does not own a second VM runtime.

The normal isolated API keeps one non-cloneable owner alive and gives agents a
clone-cheap tool handle:

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
// Pass `tool` to `Tools::builder().tool(tool)` or an agent tools factory.
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
