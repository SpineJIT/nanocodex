# Nanocodex on Vercel Workflow actors

This example runs the Nanocodex Rust/WASM agent as a multi-turn actor built from
Vercel Workflows and native Vercel Function WebSockets.

Each Workflow run is one agent session:

- a typed Workflow hook accepts prompts and processes them sequentially;
- the Workflow event log retains the latest Nanocodex `SessionSnapshot` between
  stateless Function steps;
- every accepted prompt, typed agent event, and terminal result is appended to
  the Workflow's resumable stream;
- each WebSocket Function independently tails that durable stream, so clients
  connected to different Function instances still see the same ordered output;
- reconnecting with `startIndex` resumes after the last observed event instead
  of restarting inference.

This is the Vercel equivalent of the Cloudflare Durable Object and Rivet Actor
demos. It intentionally uses Vercel's actor-pattern Workflow rather than
pretending Function memory is durable. It does not require Redis.

## Architecture

```text
browser A ─ WebSocket Function ─┐
                               ├─ replayable Workflow stream
browser B ─ WebSocket Function ─┘             │
                                              ▼
                                  one Workflow run / actor
                                  sequential prompt hook
                                              │
                                              ▼
                                  Nanocodex WASM Function step
                                     ├─ Responses WebSocket → OpenAI
                                     └─ named persistent Vercel Sandbox
                                          └─ files, commands, preview domains
```

The WebSocket connection itself is disposable and bounded by the Vercel
Function duration. The browser reconnects automatically. The Workflow run,
snapshot, prompt hook, and output stream are the durable pieces.

Every Workflow actor also owns a named Vercel Sandbox. The caller-defined
`sandbox_exec`, `sandbox_read_file`, `sandbox_write_file`,
`sandbox_list_files`, and `sandbox_preview` tools use its isolated Firecracker
VM. Persistent sandboxes automatically snapshot their filesystem when a VM
session stops and resume it on the next turn. The demo exposes ports 3000,
5173, 8000, and 8080 for previews. Vercel OIDC authenticates Sandbox SDK calls
inside the deployment, so no Vercel access token is passed to Nanocodex or the
guest VM.

## Local development

Build the repository's WASM package and install this consumer:

```sh
just build-wasm
npm ci --prefix examples/vercel-workflows
```

For API-key authentication, use Vercel CLI's local runtime because native
WebSocket upgrades require Vercel's Function adapter:

```sh
export OPENAI_API_KEY=sk-...
export NANOCODEX_AUTH_MODE=api_key
npx vercel dev examples/vercel-workflows
```

For the same local ChatGPT subscription login used by Codex:

```sh
npm run dev:subscription --prefix examples/vercel-workflows
```

The helper reads `$CODEX_HOME/auth.json` or `~/.codex/auth.json`, verifies that
the file is private and that the access token is not about to expire, and keeps
the token in the server process. `NANOCODEX_CODEX_AUTH_FILE` overrides the path.

## Deploy with subscription authentication

Authenticate Vercel CLI once with `npx vercel login`, then run from the
repository root:

```sh
npm run deploy:subscription --prefix examples/vercel-workflows
```

The deployment helper:

1. links or creates `nanocodex-vercel-workflows` (override with
   `VERCEL_PROJECT`; set `VERCEL_SCOPE` when your account belongs to multiple
   teams);
2. copies only the current Codex access token and account metadata into
   sensitive Production environment variables;
3. enables Fluid Compute and sequential Workflow replay;
4. builds and packs the current repository's Nanocodex WASM package into a
   temporary deployment directory; and
5. deploys that staged app to Production without committing generated WASM or
credentials.

Fluid Compute applies to the Next.js Functions that host WebSocket tails and
Workflow steps. Vercel Sandbox is a separate persistent Firecracker service;
the example uses both rather than treating Fluid function memory as the code
workspace.

No refresh token is copied. When the access token expires or is rejected, run
`codex login` and deploy again. To restrict creation of new sessions, set a
random `NANOCODEX_ADMIN_TOKEN` before deploying; the browser never persists
that creation token.

After deployment, prove cross-client synchronization:

```sh
export NANOCODEX_DEMO_URL=https://your-project.vercel.app
npm run multiclient --prefix examples/vercel-workflows
```

The smoke test creates one Workflow actor, connects two independent
WebSockets, submits one prompt, and requires both clients to observe its
acceptance, sandbox write/exec/read tool calls, model event stream, and terminal
result.

## Browser demo

1. Create a new Workflow.
2. Copy its `wrun_...` session ID.
3. Open the deployment in a separate browser profile and join that ID.
4. Send a prompt from either client.
5. Detach or reload during inference; both clients resume the same durable
   stream and terminal result.
6. Ask it to start a server on port 3000 and use `sandbox_preview`; open the
   returned `vercel.run` URL.

The browser stores only the session capability, a bounded transcript, active
turn metadata, and its own stream cursor. Model credentials stay in the server
step. Treat the session ID as a bearer capability: anyone who knows it can read
that session's stream and submit prompts. If `NANOCODEX_ADMIN_TOKEN` is unset,
any visitor can also create sessions and consume model tokens.

## Validation

```sh
npm run check --prefix examples/vercel-workflows
```

The example pins the current native WebSocket API, which Vercel still labels
experimental. Connections are expected to close when a Function reaches its
maximum duration; automatic cursor-based reconnection is part of the demo's
normal lifecycle.

References:

- [Vercel Workflow actor pattern](https://github.com/vercel/workflow-examples/tree/main/actors)
- [Workflow multi-turn session modeling](https://workflow-sdk.dev/docs/ai/chat-session-modeling)
- [Workflow resumable streams](https://workflow-sdk.dev/docs/ai/resumable-streams)
- [Vercel native WebSocket chat guide](https://vercel.com/kb/guide/real-time-chat-websockets)
