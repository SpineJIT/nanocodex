# Nanocodex on Cloudflare Durable Objects

This example runs the real Rust/WASM Nanocodex harness inside one SQLite-backed
Durable Object per agent session. The front Worker only authenticates session
creation and routes capability URLs; the object owns the agent, OpenAI
Responses WebSocket, client sockets, typed history, tools, and durable commits.

```text
client WebSocket ──> Worker router ──> one NanocodexSession object per session
                                         ├─ Rust/WASM Nanocodex driver
                                         ├─ persistent model WebSocket
                                         ├─ hibernatable client sockets
                                         └─ SQLite snapshot + terminal turns

ChatGPT subscription ───────────────> one NanocodexSubscriptionAuth object
                                         └─ SQLite token rotation + 401 recovery
```

Hot follow-on turns reuse the same WASM agent, cache identity, typed history,
and upstream socket. Each completed turn atomically commits its Nanocodex
snapshot and terminal client result. Failed partial turns never enter durable
history. Repeating a completed client turn ID returns the stored terminal result
without another model call; duplicates received during cold initialization are
coalesced before the first await for the same reason.

An outbound WebSocket prevents a Durable Object from hibernating while it is
retained. A one-shot idle alarm therefore shuts down Nanocodex after 30 seconds
(configurable), closing the OpenAI socket. Client WebSockets use Cloudflare's
hibernation API and remain connected. Their next command wakes the object and
resumes complete client-owned typed history from SQLite. See Cloudflare's
[Durable Object lifecycle](https://developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/)
and [WebSocket hibernation](https://developers.cloudflare.com/durable-objects/best-practices/websockets/)
documentation for the underlying behavior.

## Run locally

From the repository root, build the optimized WASM package and install the
example:

```sh
just build-wasm
npm ci --prefix examples/cloudflare-workers
```

Sign in once with Codex, then start the subscription-backed Worker:

```sh
codex login
npm run dev:subscription --prefix examples/cloudflare-workers
```

The launcher securely reads `$CODEX_HOME/auth.json` (normally
`~/.codex/auth.json`), requires it to be mode `0600`, and gives workerd only the
current access token and account metadata through a temporary mode-`0600` env
file. It never reads, copies, rotates, or persists the Codex CLI refresh token.
The temporary file is removed when Wrangler exits. If the access token expires,
run `codex login` and restart this command.

The access token is sent only by the Worker host during the upstream WebSocket
upgrade; it never crosses into WASM or a client event. Local workerd keeps the
access token behind the singleton auth Durable Object and the launcher clears
any credential retained by an older run before accepting the new login.
`GET /auth/chatgpt` reports non-secret status and `DELETE /auth/chatgpt` clears
the durable copy; both routes require the admin bearer token.

Start workerd in one terminal and run the live probes in another:

```sh
npm run repl --prefix examples/cloudflare-workers
npm run smoke --prefix examples/cloudflare-workers
npm run stress --prefix examples/cloudflare-workers
npm run soak --prefix examples/cloudflare-workers
npm run fanout --prefix examples/cloudflare-workers
```

The REPL is intentionally disposable. It stores only the session capability
URL and an unfinished turn ID/input in `.nanocodex/cloudflare-repl.json`; the
WASM agent, model socket, history, and execution remain in the Durable Object.
The file is mode `0600` because the session URL is a bearer capability. Press
Ctrl-C during inference to drop only the local WebSocket. Re-running the same
command reconnects and resubmits the idempotent turn ID, which either joins the
active turn or replays its committed terminal result. Use `/status` or `/exit`
at the prompt. Set `NANOCODEX_REPL_STATE` to isolate another local REPL state
file.

This demonstrates durable client detachment, not distributed exactly-once
inference. If the Worker process itself dies before a turn commits, reopening
the REPL resubmits the same turn from the last committed snapshot; a partial
provider response cannot be resumed.

The smoke performs a real model turn, verifies duplicate suppression, waits for
idle teardown, then proves that a follow-on remembers history after the agent
is reconstructed. `stress` drives ping round trips through one object, `soak`
checks parallel sessions for cross-session leakage and duplicate model calls,
and `fanout` broadcasts a bursty model stream to 64 attached clients. Override
their `NANOCODEX_*` environment variables to change the workload.

For deterministic transport and subscription-refresh development without model
usage, run `npm run mock:openai`, set
`OPENAI_WEBSOCKET_URL=ws://127.0.0.1:8790` in API-key mode, and run the same
probes. The mock speaks the actual streamed Responses protocol. In ChatGPT mode
it also validates the bearer/account headers and serves rotating OAuth tokens,
so the full Rust/WASM driver, snapshot, idle shutdown, restore, and auth-retry
paths execute.

If `chatgpt.com` denies the local workerd runtime even though the same login
works in Codex, use the mock for deterministic local validation and run the
real subscription smoke from a deployed Worker. Treat the deployed edge check
as the release gate.

## Validate and deploy

```sh
npm run check --prefix examples/cloudflare-workers
cd examples/cloudflare-workers
npx wrangler secret put CHATGPT_ACCESS_TOKEN
npx wrangler secret put CHATGPT_REFRESH_TOKEN
npx wrangler secret put CHATGPT_ACCOUNT_ID
npx wrangler secret put NANOCODEX_ADMIN_TOKEN
npx wrangler deploy
```

For a long-lived deployment, the singleton auth Durable Object stores the
dedicated credential in SQLite, refreshes it five minutes before expiry, and
performs one revision-guarded refresh-and-retry when an upstream upgrade returns
401. This avoids a refresh-token race when many sessions reconnect together.
Use a dedicated Codex subscription login for the deployment: refresh tokens
rotate, so sharing one between a local Codex installation and a Worker can
invalidate either copy. Do not commit, log, or paste `auth.json` wholesale;
restrict Worker and Durable Object access and follow the account's applicable
terms.

API-key mode remains available as an explicit alternative by setting
`NANOCODEX_AUTH_MODE=api_key` and providing `OPENAI_API_KEY`, but it is not used
by this demo.

`npm run check` type-checks, runs the Worker-native Durable Object suite
(including forced eviction with a live hibernatable client socket), and asks
Wrangler to build the complete WASM deployment without uploading it.

`POST /sessions` requires `Authorization: Bearer $NANOCODEX_ADMIN_TOKEN` and
returns a random session ID plus WebSocket URL. The session ID is then a bearer
capability: do not log or expose it. Production applications can replace this
small router policy with their own authentication while leaving the object and
Nanocodex lifecycle unchanged.

Client WebSocket commands are JSON objects:

```jsonc
{ "type": "prompt", "id": "client-turn-1", "input": "Hello" }
{ "type": "steer", "id": "client-turn-1", "input": "Be concise" }
{ "type": "cancel", "id": "client-turn-1" }
{ "type": "status" }
{ "type": "ping", "nonce": "health-1" }
```

The object streams contractual Nanocodex events as `{ "type": "event", ... }`
and emits exactly one application terminal message, `turn_completed` or
`turn_failed`, for each accepted client turn.
