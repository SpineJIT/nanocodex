import assert from "node:assert/strict";
import { test } from "node:test";

import { runOwnedSession } from "./session.mjs";

test("the Node example reads typed results and releases every handle", async () => {
  const harness = createHarness(["42", "43"]);
  const logs = [];
  const results = await runOwnedSession(harness.agent, {
    log: (...values) => logs.push(values),
    logDiagnostic: (value) => logs.push([value]),
  });

  assert.equal(results.first.finalMessage, "42");
  assert.equal(results.second.finalMessage, "43");
  assert.deepEqual(logs, [
    ["tool: multiply"],
    ["first:", "42"],
    ["second:", "43"],
  ]);
  assert.deepEqual(
    harness.prompts,
    [
      "Use multiply to calculate 6 × 7. Return only the number.",
      "Add one to that result. Return only the number.",
    ],
  );
  assert.deepEqual(harness.disposedTurns, [1, 1]);
  assert.equal(harness.unwatched, 1);
  assert.equal(harness.watchOffs, 1);
  assert.equal(harness.agentDisposals, 1);
});

test("a rejected result still releases the accepted Turn and agent", async () => {
  const failure = new Error("model failed");
  const harness = createHarness([failure]);

  await assert.rejects(
    runOwnedSession(harness.agent, {
      log() {},
      logDiagnostic() {},
    }),
    failure,
  );
  assert.deepEqual(harness.disposedTurns, [1]);
  assert.equal(harness.unwatched, 1);
  assert.equal(harness.watchOffs, 1);
  assert.equal(harness.agentDisposals, 1);
});

function createHarness(outputs) {
  const prompts = [];
  const disposedTurns = [];
  let unwatched = 0;
  let watchOffs = 0;
  let agentDisposals = 0;
  const agent = {
    events: {
      watch() {
        return {
          onEvent(listener) {
            listener({
              type: "tool.call",
              payload: { tool: "multiply" },
            });
            return () => {
              unwatched += 1;
            };
          },
          off() {
            watchOffs += 1;
          },
        };
      },
    },
    turn: {
      prompt({ input }) {
        prompts.push(input);
        const output = outputs[prompts.length - 1];
        const index = disposedTurns.push(0) - 1;
        return {
          async result() {
            if (output instanceof Error) throw output;
            return turnResult(output);
          },
          dispose() {
            disposedTurns[index] += 1;
          },
        };
      },
    },
    dispose() {
      agentDisposals += 1;
    },
  };
  return {
    agent,
    disposedTurns,
    prompts,
    get unwatched() {
      return unwatched;
    },
    get watchOffs() {
      return watchOffs;
    },
    get agentDisposals() {
      return agentDisposals;
    },
  };
}

function turnResult(finalMessage) {
  return {
    finalMessage,
    snapshot: {
      version: 1,
      model: "gpt-5.6-sol",
      lineage_id: "lineage",
      prompt_cache_key: "cache",
      workspace: "/workspace",
      canonical_context: {},
      history: [],
    },
    usage: {
      input_tokens: 1,
      cached_input_tokens: 0,
      cache_write_input_tokens: 0,
      output_tokens: 1,
      reasoning_output_tokens: 0,
      total_tokens: 2,
      estimated_cost: null,
      cost_status: "usage_not_reported",
    },
  };
}
