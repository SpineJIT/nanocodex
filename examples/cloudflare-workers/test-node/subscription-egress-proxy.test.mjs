import { createServer } from "node:http";
import test from "node:test";
import assert from "node:assert/strict";

import WebSocket, { WebSocketServer } from "ws";

import { startSubscriptionEgressProxy } from "../scripts/subscription-egress-proxy.mjs";

test("subscription egress proxy is capability-gated and relays headers and frames", async () => {
  const upstreamServer = createServer();
  const upstreamSockets = new WebSocketServer({ noServer: true });
  let authorization;
  upstreamServer.on("upgrade", (request, socket, head) => {
    authorization = request.headers.authorization;
    upstreamSockets.handleUpgrade(request, socket, head, (peer) => {
      peer.on("message", (message, binary) => peer.send(message, { binary }));
    });
  });
  await listen(upstreamServer);
  const address = upstreamServer.address();
  assert(address && typeof address !== "string");

  const proxy = await startSubscriptionEgressProxy({
    upstreamUrl: `ws://127.0.0.1:${address.port}/responses`,
  });
  try {
    const denied = await rejected(proxy.url.replace(/\/v1\/[^/]+$/, "/v1/wrong"));
    assert.equal(denied, 404);

    const socket = new WebSocket(proxy.url, {
      headers: {
        authorization: "Bearer test-only-token",
        "chatgpt-account-id": "account-1",
      },
    });
    await once(socket, "open");
    socket.send("hello");
    const [message] = await once(socket, "message");
    assert.equal(message.toString(), "hello");
    assert.equal(authorization, "Bearer test-only-token");
    socket.close(1000, "done");
    await once(socket, "close");
  } finally {
    await proxy.close();
    upstreamSockets.clients.forEach((socket) => socket.terminate());
    await new Promise((resolve, reject) => upstreamServer.close((error) => error ? reject(error) : resolve()));
  }
});

function listen(server) {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
}

function once(target, event) {
  return new Promise((resolve, reject) => {
    target.once(event, (...args) => resolve(args));
    target.once("error", reject);
  });
}

function rejected(url) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url, { headers: { authorization: "Bearer nope" } });
    socket.once("unexpected-response", (_request, response) => {
      resolve(response.statusCode);
      socket.terminate();
    });
    socket.once("error", reject);
  });
}
