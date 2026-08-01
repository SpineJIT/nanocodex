# nanocodex

A from-scratch Codex rewrite for the latest generation of models. The experiment is to keep the
same tools and behavior while making the runtime much smaller; nanocodex makes the implementation
and evaluation record legible.

## Stack

- Vite + React
- Cloudflare Vite plugin and Workers runtime
- Wrangler for preview and deployment
- Pierre Trees and Diffs for the file tree, source viewer, and the single virtualized commit stream
- TanStack Virtual for the commit quick-jump and evaluation indexes
- Derived job, trial, trajectory, and verifier views

The visual system follows the local Paradigm website's semantic tokens,
typography roles, grid, controls, and search treatment while using system font
fallbacks rather than the site's proprietary font files.

## Development

```bash
cd web
npm install
npm run dev
```

The homepage consumes the publishable `nanocodex` and `nanocodex-react`
packages under `../js`; it does not reach into generated WASM artifacts. Its
React integration follows an external-store pattern: create a
`createConfig()` once, pass it to `NanocodexProvider`, and consume
`useNanocodex`, `useNanocodexMessage`, or `useConfig`. React owns no agent
history, credential policy, or model-loop state.

The local Worker and Vite client run together at `https://localhost:5173`, using
the same Cloudflare Vite-plugin layout as Tempo's React MPP examples.

`npm run dev` and `npm run build` first regenerate
`src/data/harness-repository.json` from the parent repository. Override the source or
history depth with `NANOCODEX_REPO` and `NANOCODEX_COMMIT_LIMIT`. The default index
covers the complete repository history and stores it as one streamed patch
asset. The commit view parses complete files in bounded batches and appends
them to one Pierre CodeView, yielding between batches so scrolling stays
responsive.

The same sync step discovers linked worktrees and derives a compact eval index
from their retained Nanocodex and Codex jobs. It automatically pairs the largest
exact task-set match for a like-for-like comparison. Trial
details retain metrics, phase timing, verifier assertions, tool status, and
API-visible final output. Raw tool output, stderr, secrets, and hidden reasoning
are not copied into the website.

The homepage and Evals view present the matched public task set as a provisional
development instrument, not a general product ranking. The comparison links
each disagreement back to its retained trial so compatibility gaps can be
inspected directly.

### Live eval view

The Evals route switches from its static published snapshot to a live,
runner-independent task matrix when the local evidence endpoint is available.
Start the read-only server with one or more differential sweep output or parent
directories:

```bash
npm run evals:server -- \
  --static dist-evals \
  /path/to/wave-a/output/sweep \
  /path/to/wave-b/output/sweep
```

Build the standalone bundle first with `npm run evals:build`; `--static` serves
it and the API from one process. For development, use `npm run evals:dev`, or
run the complete website normally. Both proxy `/api/evals` to
`http://127.0.0.1:8788`; set `NANOCODEX_EVALS_API` to use another endpoint.
The browser receives an initial snapshot and subsequent updates over
server-sent events, with automatic reconnect after an interruption.

The server is deliberately loopback-only and is not routed by the production
Cloudflare Worker; deployed `/api/evals*` requests return 404 unless an operator
explicitly runs this separate process and supplies a same-origin proxy. It reads
durable manifests, compact comparison reports, and the latest
progress heartbeat. It folds infrastructure replacement chains back into their
original trial coordinate, ignores cleaned reportless restart tombstones, and
reports host CPU, memory, swap, and filesystem capacity. Clicking a completed
coordinate loads its task instruction, structured report, final agent messages,
and bounded canonical verifier stdout/stderr through an opaque case ID. The
server does not provide arbitrary file or workspace access and does not serve
raw prompts, reasoning, or tool transcripts. Host health is serialized only as
normalized utilization percentages; exact CPU count, memory, swap, disk size,
scheduler limits, host paths, network addresses, tailnet names, and credential
shapes are excluded or redacted. Completed reports are cached in
memory; only live progress files are reread, so watching the dashboard does not
interfere with VM disk cleanup or shared image/runtime caches.

For tailnet-only access from the eval host, keep both processes bound to
loopback and proxy the Vite port with Tailscale Serve. Inspect the host's
existing Serve configuration before adding the route.

The homepage is also a real embedded-agent demo with three deliberately thin
layers:

- `../js/bindings` publishes `nanocodex`, the viem-v3-style imperative client.
  Runtime entrypoints expose flattened `Agent.create` factories, decorated
  domain actions, standalone `Actions` namespaces, and typed watcher handles.
- `../js/react` publishes `nanocodex-react`, the wagmi-like headless React owner. Its provider and
  hooks manage the module Worker lifecycle, readiness, commands, and event
  subscriptions without imposing presentation policy.
- `AgentTerminal` is the optimized Ratatui-faithful consumer: native colors,
  rendering hierarchy, queue/steer behavior, `/btw`, historical branch editing,
  branch navigation, per-branch drafts, clipboard images, and key bindings over
  virtualized transcripts.

The module Worker loads the generated `nanocodex-wasm` package, and the Rust
engine owns the persistent Responses session, typed history, event stream, and
tool loop. The Cloudflare Worker upgrades `/api/responses` and proxies OpenAI
tool calls. It accepts a user-provided OpenAI key into a one-hour Durable Object
session and returns only an opaque `HttpOnly`, `SameSite=Strict` cookie. The key
is never placed in a URL, local storage, React state, or WASM configuration.
A user key takes precedence over the optional deployment-owned
`OPENAI_API_KEY`; forgetting or expiring it falls back to that deployment key
when present.

OpenAI remains the default agent connection. A user can explicitly select
Tempo MPP instead; only then does React lazy-load Wagmi and Tempo Accounts,
open the standard embedded Tempo Wallet dialog for its account and passkey flow, and
authorize a bounded one-day access key in that same Accounts connection
ceremony. The
module Worker hydrates that delegated signer from Accounts' IndexedDB storage
and gives it to an mppx session manager with a durable channel store. The same
MPP channel is reused across turns and reloads and is not closed by Nanocodex.
Wallet, payer, delegated signer, channel, cumulative authorization, and the
agent event JSONL are shown only while the MPP route is selected. The normal
OpenAI route does not initialize or expose any payment state.

Development uses `vite-plugin-mkcert` because the Accounts SDK intentionally
falls back to a popup on plain HTTP. Cross-origin passkeys inside the hosted
Tempo Wallet iframe require a secure context; trusted local HTTPS exercises the
same embedded flow as production.

Local development reads the optional ignored root `.env` through the repository
workflow. For a shared demo fallback, configure the deployed Worker with
`wrangler secret put OPENAI_API_KEY`. BYOK itself uses the `BYOK_SESSIONS`
Durable Object binding declared in `wrangler.jsonc` and does not require a
deployment-owned OpenAI key.

Streaming events are coalesced once per animation frame before updating the
semantic transcript, and each independently scrolling transcript is
virtualized. `npm test` keeps the
event accumulator bounded under a 20,000-delta burst and covers assistant,
reasoning, and tool lifecycle updates.

The homepage also exposes the release contract: the checksum-verifying install
command, in-place `nanocodex update`, the crates.io SDK entry point, and links
to the latest GitHub Release and grouped conventional-commit changelog. GitHub
release notes also credit each pull request contributor.

Navigation stays available whenever an input is not active: `H`, `T`, `C`, `R`,
and `E` switch between Home, Code, Commits, Requests, and Evals. The repository
homepage is the root route. In Code, `Ctrl+P` searches the left tree and `Ctrl+F` opens the
fuzzy all-file jumper. In Commits, `F` searches history. Code and commit
scrolling are left to Pierre CodeView and the browser's native input behavior.

## Production

```bash
npm run build
npm run preview
npm run deploy:preview
npm run deploy
```

The proposal endpoint is intentionally a testnet-preview `402` until a live MPP
recipient and settlement policy are configured.
