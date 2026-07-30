import { randomBytes } from "node:crypto";
import { createServer } from "node:http";

import WebSocket, { WebSocketServer } from "ws";

const DEFAULT_UPSTREAM = "wss://chatgpt.com/backend-api/codex/responses";
const MAX_BUFFERED_BYTES = 32 * 1024 * 1024;
const FORWARDED_HEADERS = [
  "authorization",
  "chatgpt-account-id",
  "openai-beta",
  "session-id",
  "thread-id",
  "user-agent",
  "x-client-request-id",
  "x-codex-turn-state",
  "x-openai-fedramp",
  "x-openai-internal-codex-responses-lite",
  "x-responsesapi-include-timing-metrics",
];

export async function startSubscriptionEgressProxy(options = {}) {
  const upstreamUrl = options.upstreamUrl ?? DEFAULT_UPSTREAM;
  const capability = randomBytes(32).toString("base64url");
  const path = `/v1/${capability}`;
  const sockets = new Set();
  const server = createServer((_request, response) => {
    response.writeHead(404, { "content-type": "text/plain", "cache-control": "no-store" });
    response.end("not found\n");
  });
  const websocketServer = new WebSocketServer({ noServer: true, maxPayload: MAX_BUFFERED_BYTES });

  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
  });
  server.on("upgrade", (request, socket, head) => {
    let pathname;
    try {
      pathname = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
    } catch {
      rejectUpgrade(socket, 400, "bad request");
      return;
    }
    if (pathname !== path) {
      rejectUpgrade(socket, 404, "not found");
      return;
    }

    const authorization = request.headers.authorization;
    if (typeof authorization !== "string" || !authorization.startsWith("Bearer ")) {
      rejectUpgrade(socket, 401, "missing authorization");
      return;
    }

    const upstream = new WebSocket(upstreamUrl, {
      handshakeTimeout: 15_000,
      headers: forwardedHeaders(request.headers),
      maxPayload: MAX_BUFFERED_BYTES,
    });
    let upgraded = false;
    upstream.once("open", () => {
      websocketServer.handleUpgrade(request, socket, head, (client) => {
        upgraded = true;
        bridge(client, upstream);
      });
    });
    upstream.once("unexpected-response", (_upstreamRequest, response) => {
      if (upgraded || socket.destroyed) return;
      rejectUpgrade(socket, response.statusCode ?? 502, "upstream WebSocket rejected");
      upstream.terminate();
    });
    upstream.once("error", () => {
      if (!upgraded && !socket.destroyed) rejectUpgrade(socket, 502, "upstream WebSocket failed");
    });
    socket.once("close", () => {
      if (!upgraded && upstream.readyState !== WebSocket.CLOSED) upstream.terminate();
    });
  });

  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(options.port ?? 0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("subscription proxy did not bind TCP");

  return {
    url: `ws://127.0.0.1:${address.port}${path}`,
    async close() {
      websocketServer.clients.forEach((client) => client.terminate());
      sockets.forEach((socket) => socket.destroy());
      await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
    },
  };
}

function forwardedHeaders(source) {
  const headers = {};
  for (const name of FORWARDED_HEADERS) {
    const value = source[name];
    if (typeof value === "string") headers[name] = value;
    else if (Array.isArray(value) && value[0]) headers[name] = value[0];
  }
  return headers;
}

function bridge(left, right) {
  let closed = false;
  const relay = (source, destination) => (data, isBinary) => {
    if (closed || destination.readyState !== WebSocket.OPEN) return;
    if (destination.bufferedAmount + data.byteLength > MAX_BUFFERED_BYTES) {
      closed = true;
      source.close(1013, "relay backpressure limit");
      destination.close(1013, "relay backpressure limit");
      return;
    }
    destination.send(data, { binary: isBinary }, (error) => {
      if (error && !closed) {
        closed = true;
        source.terminate();
        destination.terminate();
      }
    });
  };
  left.on("message", relay(left, right));
  right.on("message", relay(right, left));
  left.once("close", (code, reason) => closePeer(right, code, reason));
  right.once("close", (code, reason) => closePeer(left, code, reason));
  left.once("error", () => right.terminate());
  right.once("error", () => left.terminate());
}

function closePeer(peer, code, reason) {
  if (peer.readyState === WebSocket.OPEN) peer.close(safeCloseCode(code), reason.toString().slice(0, 120));
  else if (peer.readyState === WebSocket.CONNECTING) peer.terminate();
}

function safeCloseCode(code) {
  const standard = code >= 1000 && code <= 1014 && ![1004, 1005, 1006].includes(code);
  return standard || (code >= 3000 && code <= 4999) ? code : 1011;
}

function rejectUpgrade(socket, status, message) {
  const body = `${message}\n`;
  socket.end(
    `HTTP/1.1 ${status} ${message}\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nContent-Length: ${Buffer.byteLength(body)}\r\nConnection: close\r\n\r\n${body}`,
  );
}
