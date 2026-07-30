import { randomUUID } from "node:crypto";

import { createNanocodexClient } from "../src/client.js";

const endpoint = process.env.RIVET_PUBLIC_ENDPOINT ?? "http://127.0.0.1:6420";
const client = createNanocodexClient(endpoint);
const session = client.nanocodex.getOrCreate([
  process.env.NANOCODEX_SMOKE_ACTOR_KEY ?? "nanocodex-smoke",
]);
await session.reset();
const events = session.connect();
let eventCount = 0;
events.on("agentEvent", () => eventCount += 1);
await events.ready;

const firstRequest = {
  id: randomUUID(),
  input: "Reply with exactly EDGE_OK and nothing else.",
};
const started = performance.now();

try {
  const [first, duplicate] = await Promise.all([
    session.prompt(firstRequest),
    session.prompt(firstRequest),
  ]);
  if (first.final_message !== "EDGE_OK" || duplicate.final_message !== first.final_message) {
    throw new Error(`unexpected first turn: ${JSON.stringify(first)}`);
  }

  const replay = await session.prompt(firstRequest);
  if (replay.final_message !== first.final_message) throw new Error("terminal replay changed its result");

  await session.unload();
  const unloaded = await session.status();
  if (unloaded.agent_loaded) throw new Error("unload left the WASM driver resident");

  const restored = await session.prompt({
    id: randomUUID(),
    input: "What exact token did I ask you to return previously? Reply with only that token.",
  });
  if (restored.final_message !== "EDGE_OK") {
    throw new Error(`restored session lost history: ${restored.final_message}`);
  }
  const status = await session.status();
  console.log(JSON.stringify({
    actor_session_id: status.session_id,
    auth_mode: status.auth_mode,
    completed_turns: status.completed_turns,
    elapsed_ms: Math.round(performance.now() - started),
    events: eventCount,
    restored: status.has_snapshot,
    status: "ok",
  }));
} finally {
  await events.dispose();
  await session.reset();
}
