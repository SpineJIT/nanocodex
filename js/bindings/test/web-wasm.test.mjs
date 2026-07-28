import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import WebSocket, { WebSocketServer } from "ws";

import { Agent } from "../browser/index.mjs";

test("web-target WASM runs the shared model loop through the browser host", async () => {
  const server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  await new Promise((resolve, reject) => {
    server.once("listening", resolve);
    server.once("error", reject);
  });
  const connection = new Promise((resolve) => server.once("connection", resolve));
  const events = [];
  const wasm = await readFile(new URL("../pkg-web/nanocodex_bg.wasm", import.meta.url));
  const endpoint = `ws://127.0.0.1:${server.address().port}`;
  const agent = await Agent.create({
    apiKey: "test-key",
    WebSocketImpl: WebSocket,
    module: wasm,
    websocketUrl: endpoint,
    thinking: "low",
    sessionId: "018f1f9a-7b3c-7a07-8000-000000000007",
  });
  const watch = agent.events.watch({ includeAllSessions: true });
  watch.onEvent((event) => events.push(event));

  const scenario = (async () => {
    const socket = await connection;
    const reader = messageReader(socket);
    await reader.next();
    send(socket, { type: "response.completed", response: { id: "web-warmup", usage: null } });
    const generation = await reader.next();
    assert.equal(generation.previous_response_id, "web-warmup");
    send(socket, {
      type: "response.completed",
      response: {
        id: "web-final",
        status: "completed",
        output: [{
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: "WEB_WASM_OK" }],
        }],
        usage: null,
      },
    });
  })();

  assert.equal(await agent.turn.prompt({ input: "Reply with WEB_WASM_OK." }).result(), "WEB_WASM_OK");
  await scenario;

  const branchConnection = new Promise((resolve) => server.once("connection", resolve));
  const branch = await agent.session.fork();
  assert.notEqual(branch.sessionId, agent.sessionId);
  const branchTurn = branch.turn.prompt({ input: [
    { type: "image", image_url: "data:image/png;base64,iVBORw0KGgo=" },
    { type: "text", text: "Reply with WEB_FORK_OK." },
  ] });
  const branchSocket = await branchConnection;
  const branchReader = messageReader(branchSocket);
  const branchRequest = await branchReader.next();
  assert.equal(branchRequest.previous_response_id, undefined);
  const replay = JSON.stringify(branchRequest.input);
  assert.match(replay, /Reply with WEB_WASM_OK/);
  assert.match(replay, /WEB_WASM_OK/);
  assert.match(replay, /WEB_FORK_OK/);
  assert.match(replay, /input_image/);
  send(branchSocket, {
    type: "response.completed",
    response: {
      id: "web-branch-final",
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text: "WEB_FORK_OK" }],
      }],
      usage: null,
    },
  });
  assert.equal(await branchTurn.result(), "WEB_FORK_OK");
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(events.filter((event) => event.type === "run.completed").length, 2);

  watch.off();
  branch.dispose();
  agent.dispose();
  for (const socket of server.clients) socket.terminate();
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
});

function messageReader(socket) {
  const messages = [];
  let waiter;
  socket.on("message", (data) => {
    const value = JSON.parse(data.toString("utf8"));
    if (waiter) {
      const resolve = waiter;
      waiter = undefined;
      resolve(value);
    } else {
      messages.push(value);
    }
  });
  return {
    next() {
      if (messages.length) return Promise.resolve(messages.shift());
      return new Promise((resolve) => { waiter = resolve; });
    },
  };
}

function send(socket, value) {
  socket.send(JSON.stringify(value));
}
