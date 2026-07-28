import assert from "node:assert/strict";
import { test } from "node:test";

import { Actions } from "../index.mjs";
import {
  activateHost,
  bindHostSession,
  createAgentClient,
  defineRuntime,
  releaseHostSession,
} from "../internal.mjs";

test("the headless client exposes matching direct and standalone actions", async () => {
  const events = new Set();
  const runtime = defineRuntime({
    create: () => rawAgent("session-1"),
    subscribe(listener) {
      events.add(listener);
      return () => events.delete(listener);
    },
    decorate: (agent) => agent.extend(Actions.agentActions()),
  });
  const agent = await createAgentClient(runtime);

  const first = agent.turn.prompt({ input: "first" });
  assert.equal(await first.result(), "session-1:first");
  const second = Actions.turn.prompt(agent, { input: "second" });
  assert.equal(await Actions.turn.getResult(second), "session-1:second");

  const seen = [];
  const watch = agent.events.watch();
  const unwatch = watch.onEvent((event) => seen.push(event.type));
  for (const listener of events) {
    listener({ type: "ignored", request_id: "another-session" });
    listener({ type: "accepted", request_id: "session-1" });
  }
  unwatch();
  watch.off();
  assert.deepEqual(seen, ["accepted"]);

  const iterable = Actions.events.watch(agent);
  const iterator = iterable[Symbol.asyncIterator]();
  const next = iterator.next();
  for (const listener of events) listener({ type: "streamed", request_id: "session-1" });
  assert.deepEqual(await next, {
    done: false,
    value: { type: "streamed", request_id: "session-1" },
  });
  await iterator.return();
  iterable.off();

  const branch = await agent.session.fork({ at: first });
  assert.equal(branch.sessionId, "session-1-fork");
  assert.equal(await branch.turn.prompt({ input: "branch" }).result(), "session-1-fork:branch");

  const fresh = await agent.session.spawn();
  assert.equal(fresh.sessionId, "session-1-spawn");

  await agent.session.compact();
  await Actions.session.compact(agent);

  const extended = agent.extend((client) => ({ inspect: { session: () => client.sessionId } }));
  assert.equal(extended.inspect.session(), "session-1");
});

test("the host bridge keeps retry timing and handshake detail session-scoped", async () => {
  const sleeps = [];
  const left = {
    connect(_endpoint, _apiKey, sessionId, metadata) {
      const error = new Error(`rejected ${sessionId}`);
      error.status = 429;
      error.body = "slow down";
      error.retryAfter = 3;
      assert.deepEqual(metadata, {
        accountId: "acct-left",
        fedramp: true,
        turnState: "turn-left",
      });
      throw error;
    },
    sleep(milliseconds) {
      sleeps.push(["left", milliseconds]);
      return Promise.resolve();
    },
  };
  const right = {
    connect() {
      throw new Error("unused");
    },
    sleep(milliseconds) {
      sleeps.push(["right", milliseconds]);
      return Promise.resolve();
    },
  };

  activateHost(left);
  bindHostSession(left, "session-left");
  bindHostSession(right, "session-right");
  await globalThis.nanocodexHost.sleep("session-left", 7);
  await globalThis.nanocodexHost.sleep("session-right", 11);
  assert.deepEqual(sleeps, [["left", 7], ["right", 11]]);

  await assert.rejects(
    globalThis.nanocodexHost.connect(
      "wss://api.test",
      "secret",
      "acct-left",
      true,
      "session-left",
      "turn-left",
    ),
    (error) => {
      assert.deepEqual(JSON.parse(error), {
        kind: "handshake_rejected",
        status: 429,
        body: "slow down",
        retry_after: 3,
      });
      return true;
    },
  );

  releaseHostSession(left, "session-left");
  releaseHostSession(right, "session-right");
});

function rawAgent(sessionId) {
  return {
    sessionId,
    prompt(input) {
      return rawTurn(`${sessionId}:${input}`);
    },
    promptContent(input) {
      return rawTurn(`${sessionId}:${JSON.parse(input)[0].text}`);
    },
    async fork() {
      return rawAgent(`${sessionId}-fork`);
    },
    async forkFrom() {
      return rawAgent(`${sessionId}-fork`);
    },
    async spawn() {
      return rawAgent(`${sessionId}-spawn`);
    },
    async compact() {},
    free() {},
  };
}

function rawTurn(value) {
  return {
    async result() { return value; },
    async steer() {},
    async steerContent() {},
    async cancel() {},
    free() {},
  };
}
