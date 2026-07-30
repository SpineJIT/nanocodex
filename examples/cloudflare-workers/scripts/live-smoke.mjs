import { randomUUID } from "node:crypto";
import WebSocket from "ws";

const baseUrl = process.env.NANOCODEX_WORKER_URL ?? "http://127.0.0.1:8787";
const adminToken = process.env.NANOCODEX_ADMIN_TOKEN ?? "local-admin-token";
const terminalTimeoutMs = Number(process.env.NANOCODEX_SMOKE_TIMEOUT_MS ?? 180_000);

const session = await createSession();
const socket = new WebSocket(session.websocket_url);
const agentEvents = [];
const inbox = createInbox(socket, (message) => {
  if (message.type === "event") agentEvents.push(message.event);
});

try {
  await inbox.next((message) => message.type === "ready", 10_000);

  const firstId = randomUUID();
  const firstStarted = performance.now();
  socket.send(JSON.stringify({
    type: "prompt",
    id: firstId,
    input: "Reply with exactly EDGE_OK and nothing else.",
  }));
  socket.send(JSON.stringify({
    type: "prompt",
    id: firstId,
    input: "This in-flight duplicate must not reach the model.",
  }));
  const first = await terminal(inbox, firstId, terminalTimeoutMs);
  const firstMs = performance.now() - firstStarted;
  if (!String(first.final_message).includes("EDGE_OK")) {
    throw new Error(`first turn returned an unexpected answer: ${first.final_message}`);
  }

  socket.send(JSON.stringify({ type: "prompt", id: firstId, input: "must not run" }));
  const replay = await terminal(inbox, firstId, 10_000);
  if (replay.final_message !== first.final_message) {
    throw new Error("completed turn replay changed its terminal result");
  }

  const idleStarted = performance.now();
  const idleState = await pollState(
    (state) => state.completed_turns === 1 && state.agent_loaded === false,
    20_000,
  );
  const idleShutdownMs = performance.now() - idleStarted;

  const secondId = randomUUID();
  const secondStarted = performance.now();
  socket.send(JSON.stringify({
    type: "prompt",
    id: secondId,
    input: "What exact token did I ask you to return previously? Reply with only that token.",
  }));
  const second = await terminal(inbox, secondId, terminalTimeoutMs);
  const restoreMs = performance.now() - secondStarted;
  if (!String(second.final_message).includes("EDGE_OK")) {
    throw new Error(`restored turn lost conversation history: ${second.final_message}`);
  }

  const toolId = randomUUID();
  socket.send(JSON.stringify({
    type: "prompt",
    id: toolId,
    input: "You must call runtimeInfo exactly once. Then reply with only the runtime value returned by that tool.",
  }));
  const toolTurn = await terminal(inbox, toolId, terminalTimeoutMs);
  if (!String(toolTurn.final_message).includes("cloudflare-durable-object")) {
    throw new Error(`runtimeInfo tool returned an unexpected answer: ${toolTurn.final_message}`);
  }
  const toolCall = agentEvents.find((event) =>
    event.type === "tool.call" && event.payload?.tool === "runtimeInfo");
  const toolResult = agentEvents.find((event) =>
    event.type === "tool.result" && event.payload?.call_id === toolCall?.payload?.call_id);
  if (!toolCall || !toolResult || toolResult.payload?.status !== "completed") {
    throw new Error("runtimeInfo did not produce a completed tool.call/tool.result event pair");
  }

  const finalState = await state();
  if (finalState.completed_turns !== 3 || finalState.has_snapshot !== true) {
    throw new Error(`unexpected final state: ${JSON.stringify(finalState)}`);
  }

  const authStatus = finalState.auth_mode === "chatgpt" ? await subscriptionStatus() : undefined;
  console.log(JSON.stringify({
    session_id: session.session_id,
    first_turn_ms: Math.round(firstMs),
    idle_shutdown_ms: Math.round(idleShutdownMs),
    restored_turn_ms: Math.round(restoreMs),
    agent_events: agentEvents.length,
    tool_call: toolCall.payload.tool,
    completed_turns: finalState.completed_turns,
    idle_state: idleState.agent_loaded ? "loaded" : "unloaded",
    ...(authStatus === undefined ? {} : { auth_revision: authStatus.revision }),
    status: "ok",
  }));
} finally {
  inbox.close();
  socket.close(1000, "smoke complete");
  await fetch(`${baseUrl}/sessions/${session.session_id}`, { method: "DELETE" }).catch(() => {});
}

async function createSession() {
  const response = await fetch(`${baseUrl}/sessions`, {
    method: "POST",
    headers: { authorization: `Bearer ${adminToken}` },
  });
  if (!response.ok) throw new Error(`session creation failed with HTTP ${response.status}: ${await response.text()}`);
  return response.json();
}

async function state() {
  const response = await fetch(`${baseUrl}/sessions/${session.session_id}`);
  if (!response.ok) throw new Error(`state failed with HTTP ${response.status}: ${await response.text()}`);
  return response.json();
}

async function subscriptionStatus() {
  const response = await fetch(`${baseUrl}/auth/chatgpt`, {
    headers: { authorization: `Bearer ${adminToken}` },
  });
  if (!response.ok) throw new Error(`auth status failed with HTTP ${response.status}: ${await response.text()}`);
  return response.json();
}

async function pollState(predicate, timeoutMs) {
  const deadline = performance.now() + timeoutMs;
  while (performance.now() < deadline) {
    const current = await state();
    if (predicate(current)) return current;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`state did not reach the expected condition within ${timeoutMs}ms`);
}

async function terminal(inbox, id, timeoutMs) {
  const message = await inbox.next(
    (candidate) => ["turn_completed", "turn_failed"].includes(candidate.type) && candidate.id === id,
    timeoutMs,
  );
  if (message.type === "turn_failed") throw new Error(`turn ${id} failed: ${message.error}`);
  return message;
}

function createInbox(ws, observe = () => {}) {
  const queued = [];
  const waiters = [];
  const onMessage = (data) => {
    const message = JSON.parse(String(data));
    observe(message);
    const index = waiters.findIndex(({ predicate }) => predicate(message));
    if (index === -1) queued.push(message);
    else waiters.splice(index, 1)[0].resolve(message);
  };
  ws.on("message", onMessage);
  return {
    next(predicate, timeoutMs) {
      const index = queued.findIndex(predicate);
      if (index !== -1) return Promise.resolve(queued.splice(index, 1)[0]);
      return new Promise((resolve, reject) => {
        const waiter = { predicate, resolve };
        waiters.push(waiter);
        const timer = setTimeout(() => {
          const pending = waiters.indexOf(waiter);
          if (pending !== -1) waiters.splice(pending, 1);
          reject(new Error(`WebSocket message timed out after ${timeoutMs}ms`));
        }, timeoutMs);
        waiter.resolve = (message) => {
          clearTimeout(timer);
          resolve(message);
        };
      });
    },
    close() {
      ws.off("message", onMessage);
    },
  };
}
