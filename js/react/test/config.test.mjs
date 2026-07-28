import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { createConfig } from "../config.mjs";

test("the React consumer accepts the workspace Nanocodex package", () => {
  const reactPackage = JSON.parse(
    readFileSync(new URL("../package.json", import.meta.url)),
  );
  const nanocodexPackage = JSON.parse(
    readFileSync(new URL("../../bindings/package.json", import.meta.url)),
  );

  assert.equal(
    reactPackage.peerDependencies.nanocodex,
    `^${nanocodexPackage.version}`,
  );
});

test("Nanocodex config owns worker lifecycle outside React", async () => {
  const commands = [];
  let terminated = false;
  const worker = {
    onmessage: null,
    postMessage(command) { commands.push(command); },
    terminate() { terminated = true; },
  };
  const config = createConfig({
    worker: () => worker,
    thinking: "high",
    reasoningMode: "pro",
  });
  let stateChanges = 0;
  const unsubscribe = config.subscribe(() => { stateChanges += 1; });
  const messages = [];
  config.subscribeMessages((message) => messages.push(message.type));

  const unmount = config.mount();
  assert.deepEqual(commands, [{ type: "start", thinking: "high", reasoningMode: "pro" }]);
  worker.onmessage({ data: { type: "ready" } });
  await Promise.resolve();
  assert.deepEqual(config.getSnapshot(), { status: "ready", error: undefined });
  assert.deepEqual(messages, ["ready"]);
  config.dispatch({ type: "prompt", prompt: "hello" });
  assert.deepEqual(commands.at(-1), { type: "prompt", prompt: "hello" });

  unmount();
  unsubscribe();
  assert.equal(terminated, true);
  assert.equal(config.getSnapshot().status, "idle");
  assert.throws(() => config.dispatch({ type: "prompt" }), /not running/);
  assert.ok(stateChanges >= 2);
});

test("the library requires the application to provide its Worker boundary", () => {
  assert.throws(() => createConfig(), /requires worker/);
});

test("the default start command uses high reasoning", () => {
  const commands = [];
  const config = createConfig({
    worker: () => ({
      onmessage: null,
      postMessage(command) { commands.push(command); },
      terminate() {},
    }),
  });

  const unmount = config.mount();
  assert.deepEqual(commands, [
    { type: "start", thinking: "high", reasoningMode: "standard" },
  ]);
  unmount();
});

test("deferred configs start and restart with application-owned commands", () => {
  const workers = [];
  const config = createConfig({
    autoStart: false,
    worker() {
      const commands = [];
      const worker = {
        commands,
        terminated: false,
        onmessage: null,
        postMessage(command) { commands.push(command); },
        terminate() { worker.terminated = true; },
      };
      workers.push(worker);
      return worker;
    },
  });

  const unmount = config.mount();
  assert.equal(config.getSnapshot().status, "idle");
  assert.equal(workers.length, 0);

  config.start({ type: "start", transport: "mpp" });
  assert.deepEqual(workers[0].commands, [{ type: "start", transport: "mpp" }]);
  config.restart({ type: "start", transport: "openai" });
  assert.equal(workers[0].terminated, true);
  assert.deepEqual(workers[1].commands, [{ type: "start", transport: "openai" }]);

  config.disconnect();
  assert.equal(workers[1].terminated, true);
  assert.equal(config.getSnapshot().status, "idle");

  config.start({ type: "start", transport: "mpp" });

  unmount();
  assert.equal(workers[2].terminated, true);
});
