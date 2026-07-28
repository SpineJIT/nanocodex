import assert from "node:assert/strict";

const react = await import("nanocodex-react");
const config = await import("nanocodex-react/config");

assert.equal(typeof react.NanocodexProvider, "function");
assert.equal(typeof react.createConfig, "function");
assert.equal(react.createConfig, config.createConfig);
