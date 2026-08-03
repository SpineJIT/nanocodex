import WebSocket from "ws";

const baseUrl = new URL(process.env.NANOCODEX_DEMO_URL ?? "http://127.0.0.1:3000");
const adminToken = process.env.NANOCODEX_ADMIN_TOKEN?.trim();
const create = await fetch(new URL("/api/sessions", baseUrl), {
  method: "POST",
  headers: adminToken ? { authorization: `Bearer ${adminToken}` } : {},
});
const created = await create.json();
if (!create.ok) throw new Error(created?.error?.message ?? `session creation failed with HTTP ${create.status}`);
const sessionId = created.session_id;
const clients = await Promise.all([openClient(sessionId), openClient(sessionId)]);
try {
  await Promise.all(clients.map((client) => client.waitFor((message) => message.type === "ready")));
  const turnId = crypto.randomUUID();
  const input = "Use sandbox_write_file to write VERCEL_SANDBOX_OK to probe.txt, use sandbox_exec to run cat on it, and verify it with sandbox_read_file. Then reply with exactly VERCEL_SANDBOX_OK.";
  const prompt = await fetch(
    new URL(`/api/sessions/${encodeURIComponent(sessionId)}/prompt`, baseUrl),
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ id: turnId, input }),
    },
  );
  if (!prompt.ok) {
    const body = await prompt.text();
    throw new Error(`prompt failed with HTTP ${prompt.status}: ${body}`);
  }
  const observations = await Promise.all(clients.map(async (client) => {
    await client.waitFor((message) => message.type === "turn_accepted" && message.id === turnId);
    await client.waitFor((message) => message.type === "event" && message.turn_id === turnId);
    const completed = await client.waitFor(
      (message) => message.type === "turn_completed" && message.id === turnId,
      180_000,
    );
    return {
      final_message: completed.final_message,
      events: client.events.filter((event) => event.type === "event" && event.turn_id === turnId).length,
      tools: client.events
        .filter((event) => event.type === "event"
          && event.turn_id === turnId
          && event.event?.type === "tool.call")
        .map((event) => event.event.payload?.tool),
    };
  }));
  if (observations.some((result) => result.final_message.trim() !== "VERCEL_SANDBOX_OK")) {
    throw new Error(`unexpected terminal messages: ${JSON.stringify(observations)}`);
  }
  const requiredTools = ["sandbox_write_file", "sandbox_exec", "sandbox_read_file"];
  if (observations.some((result) => requiredTools.some((tool) => !result.tools.includes(tool)))) {
    throw new Error(`a synchronized client missed sandbox tool events: ${JSON.stringify(observations)}`);
  }
  process.stdout.write(`${JSON.stringify({
    session_id: sessionId,
    accepted_clients: clients.length,
    completed_clients: observations.length,
    event_counts: observations.map((result) => result.events),
    tool_calls: requiredTools,
    status: "ok",
  })}\n`);
} finally {
  for (const client of clients) client.close();
}

async function openClient(sessionId) {
  const socketUrl = new URL("/api/ws", baseUrl);
  socketUrl.protocol = baseUrl.protocol === "https:" ? "wss:" : "ws:";
  socketUrl.searchParams.set("sessionId", sessionId);
  socketUrl.searchParams.set("startIndex", "0");
  const socket = new WebSocket(socketUrl);
  const records = [];
  const waiters = new Set();
  socket.on("message", (encoded) => {
    const record = JSON.parse(encoded.toString());
    if (record.type !== "stream_event") return;
    records.push(record.event);
    for (const waiter of waiters) waiter(record.event);
  });
  await new Promise((resolveOpen, rejectOpen) => {
    const timeout = setTimeout(() => rejectOpen(new Error("WebSocket open timed out")), 20_000);
    socket.once("open", () => {
      clearTimeout(timeout);
      resolveOpen();
    });
    socket.once("error", rejectOpen);
  });
  return {
    events: records,
    close: () => socket.close(),
    waitFor(predicate, timeoutMs = 30_000) {
      const existing = records.find(predicate);
      if (existing) return Promise.resolve(existing);
      return new Promise((resolveEvent, rejectEvent) => {
        const timeout = setTimeout(() => {
          waiters.delete(observe);
          rejectEvent(new Error("timed out waiting for synchronized workflow event"));
        }, timeoutMs);
        const observe = (event) => {
          if (!predicate(event)) return;
          clearTimeout(timeout);
          waiters.delete(observe);
          resolveEvent(event);
        };
        waiters.add(observe);
      });
    },
  };
}
