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

  const firstTurn = agent.turn.prompt({ input: "first" });
  const first = await firstTurn.result();
  assert.equal(first.finalMessage, "session-1:first");
  assert.deepEqual(Object.getOwnPropertySymbols(agent), []);
  assert.deepEqual(Object.getOwnPropertySymbols(firstTurn), []);
  assert.deepEqual(Object.getOwnPropertySymbols(first), []);
  assert.equal(Object.isFrozen(first), true);
  assert.equal(Object.isFrozen(first.usage), true);
  assert.equal(Object.isFrozen(first.snapshot), true);
  assert.strictEqual(Actions.turn.getUsage(first), first.usage);
  assert.strictEqual(Actions.turn.getSnapshot(first), first.snapshot);
  const secondTurn = Actions.turn.prompt(agent, { input: "second" });
  const second = await Actions.turn.getResult(secondTurn);
  assert.equal(second.finalMessage, "session-1:second");

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
  assert.equal(
    (await branch.turn.prompt({ input: "branch" }).result()).finalMessage,
    "session-1-fork:branch",
  );

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

test("event iterators release subscriptions and fail closed before buffering without bound", async () => {
  const subscriptions = new Set();
  const runtime = defineRuntime({
    create: () => rawAgent("session-events"),
    subscribe(listener) {
      subscriptions.add(listener);
      return () => subscriptions.delete(listener);
    },
    decorate: (agent) => agent.extend(Actions.agentActions()),
  });
  const agent = await createAgentClient(runtime);
  const watch = agent.events.watch();
  const iterator = watch[Symbol.asyncIterator]();

  assert.equal(subscriptions.size, 1);
  for (let seq = 1; seq <= 4_097; seq += 1) {
    for (const listener of subscriptions) {
      listener({ type: "api.event", request_id: agent.sessionId, seq });
    }
  }
  for (let seq = 1; seq <= 4_096; seq += 1) {
    assert.equal((await iterator.next()).value.seq, seq);
  }
  await assert.rejects(iterator.next(), /event iterator exceeded its private buffer/);
  assert.equal(subscriptions.size, 0);

  const restarted = watch[Symbol.asyncIterator]();
  assert.equal(subscriptions.size, 1);
  const firstPending = restarted.next();
  const secondPending = restarted.next();
  for (const listener of subscriptions) {
    listener({ type: "api.event", request_id: agent.sessionId, seq: 4_098 });
    listener({ type: "api.event", request_id: agent.sessionId, seq: 4_099 });
  }
  assert.deepEqual(
    (await Promise.all([firstPending, secondPending])).map(({ value }) => value.seq),
    [4_098, 4_099],
  );
  await restarted.return();
  assert.equal(subscriptions.size, 0);

  watch.off();
  agent.dispose();
});

test("a failing event listener is reported without interrupting other observers", async () => {
  const subscriptions = new Set();
  const reported = [];
  const previousReportError = globalThis.reportError;
  globalThis.reportError = (error) => reported.push(error);
  try {
    const runtime = defineRuntime({
      create: () => rawAgent("session-observers"),
      subscribe(listener) {
        subscriptions.add(listener);
        return () => subscriptions.delete(listener);
      },
      decorate: (agent) => agent.extend(Actions.agentActions()),
    });
    const agent = await createAgentClient(runtime);
    const watch = agent.events.watch();
    watch.onEvent(() => { throw new Error("observer failed"); });
    const seen = [];
    watch.onEvent((event) => seen.push(event.seq));
    const iterator = watch[Symbol.asyncIterator]();
    const next = iterator.next();

    for (const listener of subscriptions) {
      listener({ type: "api.event", request_id: agent.sessionId, seq: 1 });
    }
    assert.deepEqual(seen, [1]);
    assert.equal((await next).value.seq, 1);
    assert.match(reported[0]?.message, /observer failed/);

    watch.off();
    agent.dispose();
  } finally {
    if (previousReportError === undefined) delete globalThis.reportError;
    else globalThis.reportError = previousReportError;
  }
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
    async result() {
      return {
        finalMessage: value,
        snapshot() {
          return JSON.stringify({
            version: 1,
            model: "gpt-5.6-sol",
            lineage_id: "test-lineage",
            prompt_cache_key: "test-cache-key",
            workspace: ".",
            canonical_context: {},
            history: [],
          });
        },
        usage() {
          return JSON.stringify({
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 0,
            estimated_cost: null,
            cost_status: "usage_not_reported",
          });
        },
        free() {},
      };
    },
    async steer() {},
    async steerContent() {},
    async cancel() {},
    free() {},
  };
}
