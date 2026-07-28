import assert from "node:assert/strict";

const tui = await import("nanocodex-tui-react");

assert.equal(typeof tui.NanocodexTui, "function");
