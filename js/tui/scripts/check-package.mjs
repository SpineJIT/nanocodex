import assert from "node:assert/strict";

const tui = await import("nanocodex-tui");

assert.equal(typeof tui.applyAgentEvents, "function");
assert.equal(typeof tui.initialTerminalState, "function");
assert.equal(typeof tui.queuePrompt, "function");
assert.equal(typeof tui.turnFinished, "function");
