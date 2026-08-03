import { createServer } from "node:http";
import { WebSocket, WebSocketServer } from "ws";

const port = Number(process.env.NANOCODEX_MOCK_OPENAI_PORT ?? 8790);
const responseDelayMs = Number(process.env.NANOCODEX_MOCK_DELAY_MS ?? 0);
const accountId = process.env.NANOCODEX_MOCK_CHATGPT_ACCOUNT_ID ?? "local-chatgpt-account";
if (!Number.isFinite(responseDelayMs) || responseDelayMs < 0 || responseDelayMs > 60_000) {
  throw new Error("NANOCODEX_MOCK_DELAY_MS must be between 0 and 60000");
}
const accessToken = jwt({ exp: Math.floor(Date.now() / 1000) + 3_600 });
const idToken = jwt({
  exp: Math.floor(Date.now() / 1000) + 3_600,
  "https://api.openai.com/auth": {
    chatgpt_account_id: accountId,
    chatgpt_account_is_fedramp: false,
  },
});
const server = createServer(async (request, response) => {
  if (request.method !== "POST" || request.url !== "/oauth/token") {
    response.writeHead(404).end();
    return;
  }
  const body = await readBody(request, 16 * 1024);
  const refresh = JSON.parse(body);
  if (refresh.grant_type !== "refresh_token" || refresh.refresh_token !== currentRefreshToken) {
    response.writeHead(401, { "content-type": "application/json" });
    response.end(JSON.stringify({ error: { code: "refresh_token_invalidated" } }));
    return;
  }
  currentRefreshToken = `local-rotated-refresh-token-${++refreshCount}`;
  response.writeHead(200, { "content-type": "application/json" });
  response.end(JSON.stringify({
    access_token: accessToken,
    refresh_token: currentRefreshToken,
    id_token: idToken,
  }));
});
const sockets = new WebSocketServer({ noServer: true });
let nextResponse = 1;
let currentRefreshToken = "local-refresh-token";
let refreshCount = 0;

server.on("upgrade", (request, socket, head) => {
  const subscription = request.url?.includes("/backend-api/codex/responses");
  if (subscription && (
    request.headers.authorization !== `Bearer ${accessToken}`
    || request.headers["chatgpt-account-id"] !== accountId
  )) {
    socket.end("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
    return;
  }
  sockets.handleUpgrade(request, socket, head, (websocket) => {
    sockets.emit("connection", websocket, request);
  });
});

sockets.on("connection", (socket) => {
  socket.on("message", async (data) => {
    const request = JSON.parse(data.toString("utf8"));
    const encoded = JSON.stringify(request.input ?? []);
    const exactToken = encoded.match(/Reply with exactly ([A-Z0-9_-]{1,128})/)?.[1];
    const burstCount = Number(encoded.match(/Emit BURST_(\d{1,4})/)?.[1] ?? 0);
    const hasUserPrompt = encoded.includes("Reply with exactly EDGE_OK")
      || encoded.includes("What exact token did I ask you to return previously?");
    for (let index = 0; index < burstCount; index += 1) {
      socket.send(JSON.stringify({
        type: "response.output_text.delta",
        output_index: 0,
        content_index: 0,
        delta: "x",
      }));
    }
    const response = {
      id: `mock-response-${nextResponse++}`,
      status: "completed",
      ...(exactToken || hasUserPrompt || burstCount > 0 ? {
        output: [{
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: exactToken ?? (burstCount > 0 ? "BURST_OK" : "EDGE_OK") }],
        }],
      } : {}),
      usage: null,
    };
    if (responseDelayMs > 0) {
      await new Promise((resolve) => setTimeout(resolve, responseDelayMs));
    }
    if (socket.readyState !== WebSocket.OPEN) return;
    socket.send(JSON.stringify({ type: "response.completed", response }));
  });
});

await new Promise((resolve, reject) => {
  server.listen(port, "127.0.0.1", resolve);
  server.once("error", reject);
});
console.log(`mock Responses + OAuth server listening on http://127.0.0.1:${port}`);

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    for (const socket of sockets.clients) socket.terminate();
    sockets.close();
    server.close(() => process.exit(0));
  });
}

function jwt(payload) {
  const encode = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
  return `${encode({ alg: "none" })}.${encode(payload)}.local`;
}

async function readBody(request, limit) {
  const chunks = [];
  let bytes = 0;
  for await (const chunk of request) {
    bytes += chunk.length;
    if (bytes > limit) throw new Error(`request exceeded ${limit} bytes`);
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}
