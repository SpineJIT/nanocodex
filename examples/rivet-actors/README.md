# Nanocodex on Rivet Actors

This example runs the real Rust/WASM Nanocodex harness as a durable
[Rivet Actor](https://rivet.dev/docs/actors/). Nanocodex is already an agent
harness, so it runs directly in the actor host with one owner for model history,
tools, retries, and cancellation. The example depends directly on RivetKit and
does not install `@rivet-dev/agentos`, Pi, or another agent harness. RivetKit
itself currently retains a separate legacy `@rivet-dev/agent-os-core` runtime
dependency; it has no Pi packages in its dependency tree.

## Architecture

Each `nanocodex` actor owns one conversation:

- the live WASM driver, event watcher, and turns live in ephemeral `c.vars`;
- the typed Nanocodex snapshot and terminal idempotency records live in the
  actor's embedded SQLite database;
- a completed turn is transactionally committed before its action returns or
  broadcasts `turnCompleted`;
- duplicate turn IDs share one in-flight promise or replay the stored terminal
  result without another model call;
- `onSleep` and `onDestroy` cancel turns, close the Responses WebSocket, and
  release WASM resources;
- the opaque Rivet actor ID is deterministically projected to a stable UUID for
  Nanocodex's session contract.

Actions execute in parallel in Rivet. The actor therefore caps fan-in at 16
turns, distinguishes conflicting idempotency keys by a SHA-256 input digest,
and keeps each prompt action awake through its complete model turn.

The singleton `nanocodexAuth` actor owns ChatGPT subscription credentials. It
persists rotating refresh tokens in its own SQLite database, refreshes five
minutes early, single-flights concurrent refreshes, and retries one WebSocket
upgrade after a revision-guarded 401 recovery. Bearer credentials remain in
host code and never enter Nanocodex WASM.

## Build and run

Build the repository's browser-compatible WASM package first:

```sh
just build-wasm
npm ci --prefix examples/rivet-actors
npm run check --prefix examples/rivet-actors
```

The dependency check fails if the lockfile acquires AgentOS's Pi adapter or any
of the upstream Pi harness packages.

For API-key authentication, set:

```sh
export NANOCODEX_AUTH_MODE=api_key
export OPENAI_API_KEY=sk-...
export RIVET__file_system__path=/tmp/nanocodex-rivet-demo/engine-db
npm run dev --prefix examples/rivet-actors
```

The explicit database path isolates the demo from other local Rivet versions
and persists actor state across server restarts. Do not point an older Rivet
Engine at a database created by a newer version.

In another terminal:

```sh
npm run repl --prefix examples/rivet-actors
npm run smoke --prefix examples/rivet-actors
npm run stress --prefix examples/rivet-actors
npm run brutalize --prefix examples/rivet-actors
```

The REPL is intentionally disposable. It stores only the Rivet endpoint,
actor key, and an unfinished turn ID/input in `.nanocodex/rivet-repl.json`.
The file is mode `0600` because it may contain unfinished prompt text.
The `start` action transfers ownership to the actor with `keepAwake` before it
returns; the REPL then waits through the ordinary idempotent `prompt` action.
Pressing Ctrl-C kills only that waiter. Re-running the command with the same
`NANOCODEX_REPL_SESSION` reattaches to the active turn or replays its committed
result, then continues the actor's durable conversation. Use `/status` or
`/exit` at the prompt. Set `NANOCODEX_REPL_STATE` to isolate another local
REPL state file.

This demonstrates durable client detachment, not distributed exactly-once
inference. If the actor host itself dies before a turn commits, reopening the
REPL resubmits the same turn from the last committed snapshot; a partial
provider response cannot be resumed.

The local Rivet endpoint is `http://127.0.0.1:6420`. Set
`RIVET_PUBLIC_ENDPOINT` for another deployment.

The stress driver reuses persistent actor connections and bounds fan-out to
avoid benchmarking the gateway's per-route rate limiter. Tune
`NANOCODEX_STRESS_ACTORS`, `NANOCODEX_STRESS_REPLAYS`, and
`NANOCODEX_STRESS_CONCURRENCY_PER_ACTOR` when sizing a deployment.
The longer `brutalize` soak reconnects every client between seeding and replay,
resets a bounded actor pool after each wave, and reports replay latency
percentiles from a constant-memory histogram plus best/worst wave throughput.
Tune its corresponding `NANOCODEX_SOAK_*` variables for larger runs. Set
`NANOCODEX_STRESS_KEYSPACE`
or `NANOCODEX_SOAK_KEYSPACE` when running multiple drivers concurrently; the
stable defaults prevent repeated local runs from accumulating actor records.

## ChatGPT subscription authentication

Subscription mode does not require `OPENAI_API_KEY`:

```sh
export NANOCODEX_AUTH_MODE=chatgpt
export NANOCODEX_AUTH_ACTOR_KEY=my-deployment-subscription
export NANOCODEX_AUTH_CAPABILITY=a-separate-random-secret-of-at-least-32-bytes
export CHATGPT_ACCESS_TOKEN=...
export CHATGPT_REFRESH_TOKEN=...
export CHATGPT_ACCOUNT_ID=...
export RIVET__file_system__path=/tmp/nanocodex-rivet-demo/engine-db
npm run dev --prefix examples/rivet-actors
```

`CHATGPT_FEDRAMP=true` and `CHATGPT_TOKEN_ENDPOINT` are optional. Set
`NANOCODEX_AUTH_ACTOR_KEY` to a stable key unique to the deployment; the local
fallback is `subscription`. Reusing a key intentionally resumes the persisted
rotating credential instead of reseeding it from environment variables.
`NANOCODEX_AUTH_CAPABILITY` is required in subscription mode. Every credential
action receives a short-lived, operation-bound HMAC proof with replay defense;
the capability itself never crosses the actor RPC boundary. Keep it separate
from the actor key.

ChatGPT refresh tokens rotate. Use credentials dedicated to this deployment;
do not share the same refresh token with a local Codex installation or another
deployment. Protect the auth actor and the Rivet endpoint with application
authentication before exposing them publicly. The example intentionally never
returns access or refresh tokens from its status actions.

## Events and lifecycle

Subscribe before prompting:

```ts
const session = client.nanocodex.getOrCreate(["conversation"]);
const connection = session.connect();
connection.on("agentEvent", (event) => console.log(event));
connection.on("turnCompleted", (result) => console.log(result));

await session.prompt({ id: crypto.randomUUID(), input: "Hello" });
await session.unload(); // snapshot remains durable
await connection.dispose();
```

Use `reset()` to delete the conversation snapshot and terminal replay records.
Rivet automatically sleeps idle actors after 30 seconds.

## Deployment

Rivet Actors can run on Rivet Compute or a self-hosted Rivet platform. Build
Nanocodex WASM as part of the image, start `src/server.ts`, and provide the
selected authentication secrets to the runner. Rivet's current CLI deploy path
is:

```sh
npx @rivetkit/cli deploy --token cloud_api_xxxxx
```

See the official [Rivet deployment guide](https://rivet.dev/docs/deploy/).
