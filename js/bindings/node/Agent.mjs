import { createRequire } from "node:module";
import initWeb, { Nanocodex as WebNanocodex } from "../pkg-web/nanocodex.js";

import { agentActions } from "../actions/index.mjs";
import {
  activateHost,
  bindHostSession,
  createAgentClient,
  createEventChannel,
  defineRuntime,
  releaseHostSession,
  toWasmConfig,
} from "../internal.mjs";
import { createNodeHost } from "./host.mjs";

let initializedWeb;
let NodeNanocodex;

export function create(options = {}) {
  const {
    thinking,
    reasoningMode,
    fastMode,
    instructions,
    sessionId,
    workspace,
    resume,
    apiKey,
    mpp,
    websocketUrl,
    apiBaseUrl,
    module,
    tools,
  } = options;
  const events = createEventChannel();
  if (mpp !== undefined && apiKey !== undefined) {
    throw new TypeError("apiKey and mpp are mutually exclusive");
  }
  const host = createNodeHost({
    mpp,
    onEvent: events.emit,
    tools,
    workspace: workspace ?? resume?.workspace,
  });
  activateHost(host);
  const runtime = defineRuntime({
    key: "node-wasm",
    name: "Nanocodex Node WASM",
    type: "node",
    async create(config) {
      activateHost(host);
      const Nanocodex = module === undefined
        ? loadNodeNanocodex()
        : await loadWebNanocodex(module);
      activateHost(host);
      return new Nanocodex(JSON.stringify(toWasmConfig({
        apiKey: apiKey ?? (mpp === undefined ? undefined : "mpp-managed"),
        websocketUrl: websocketUrl ?? (mpp === undefined
          ? undefined
          : "wss://openai.mpp.tempo.xyz/v1/responses"),
        apiBaseUrl,
        ...config,
      })));
    },
    subscribe: events.subscribe,
    adopt: (raw) => bindHostSession(host, raw.sessionId),
    release: (raw) => releaseHostSession(host, raw.sessionId),
    decorate: (agent) => agent.extend(agentActions()),
  });
  return createAgentClient(runtime, {
    thinking,
    reasoningMode,
    fastMode,
    instructions,
    sessionId,
    workspace,
    resume,
  });
}

function loadNodeNanocodex() {
  const require = createRequire(import.meta.url);
  NodeNanocodex ||= require("../pkg-node/nanocodex.js").Nanocodex;
  return NodeNanocodex;
}

async function loadWebNanocodex(module) {
  initializedWeb ||= initWeb({ module_or_path: module });
  await initializedWeb;
  return WebNanocodex;
}
