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
const sandboxToolCalls = new Map<string, string>();
const sandboxToolsCompleted = new Set<string>();
events.on("agentEvent", (event) => {
  eventCount += 1;
  if (event.type === "tool.call" && typeof event.payload.tool === "string") {
    sandboxToolCalls.set(String(event.payload.call_id), event.payload.tool);
  }
  if (event.type === "tool.result" && event.payload.status === "completed") {
    const tool = sandboxToolCalls.get(String(event.payload.call_id));
    if (tool) sandboxToolsCompleted.add(tool);
  }
});
await events.ready;

const firstRequest = {
  id: randomUUID(),
  input: "Reply with exactly EDGE_OK and nothing else.",
};
const started = performance.now();

try {
  const [first, duplicate] = await Promise.all([
    session.turn(firstRequest),
    session.turn(firstRequest),
  ]);
  if (first.final_message !== "EDGE_OK" || duplicate.final_message !== first.final_message) {
    throw new Error(`unexpected first turn: ${JSON.stringify(first)}`);
  }

  const replay = await session.turn(firstRequest);
  if (replay.final_message !== first.final_message) throw new Error("terminal replay changed its result");

  await session.unload();
  const unloaded = await session.status();
  if (unloaded.agent_loaded) throw new Error("unload left the WASM driver resident");

  const restored = await session.turn({
    id: randomUUID(),
    input: "What exact token did I ask you to return previously? Reply with only that token.",
  });
  if (restored.final_message !== "EDGE_OK") {
    throw new Error(`restored session lost history: ${restored.final_message}`);
  }
  const toolTurn = await session.turn({
    id: randomUUID(),
    input: "Use sandbox_write_file to write SANDBOX_OK to probe.txt, use sandbox_exec to run cat on it, and verify it with sandbox_read_file. Then reply with exactly SANDBOX_OK.",
  });
  const requiredTools = ["sandbox_write_file", "sandbox_exec", "sandbox_read_file"];
  if (toolTurn.final_message.trim() !== "SANDBOX_OK"
    || requiredTools.some((tool) => !sandboxToolsCompleted.has(tool))) {
    throw new Error(`sandbox tool proof failed: ${toolTurn.final_message}`);
  }
  const status = await session.status();
  console.log(JSON.stringify({
    actor_session_id: status.session_id,
    auth_mode: status.auth_mode,
    completed_turns: status.completed_turns,
    elapsed_ms: Math.round(performance.now() - started),
    events: eventCount,
    tool_calls: requiredTools,
    restored: status.has_snapshot,
    status: "ok",
  }));
} finally {
  await events.dispose();
  await session.reset();
}
