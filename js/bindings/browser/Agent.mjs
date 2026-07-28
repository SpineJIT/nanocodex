import init, { Nanocodex } from "../pkg-web/nanocodex.js";

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
import { createBrowserHost } from "./host.mjs";

let initialized;

export function create(options = {}) {
  const {
    apiKey,
    mpp,
    websocketUrl,
    apiBaseUrl,
    module,
    thinking,
    reasoningMode,
    fastMode,
    instructions,
    sessionId,
    workspace,
    resume,
    WebSocketImpl,
    createWebSocket,
    tools,
  } = options;
  if (mpp !== undefined && apiKey !== undefined) {
    throw new TypeError("apiKey and mpp are mutually exclusive");
  }
  const events = createEventChannel();
  const host = createBrowserHost({
    WebSocketImpl,
    createWebSocket,
    mpp,
    onEvent: events.emit,
    tools,
  });
  activateHost(host);
  const runtime = defineRuntime({
    key: "browser-wasm",
    name: "Nanocodex Browser WASM",
    type: "browser",
    async create(config) {
      activateHost(host);
      initialized ||= module === undefined ? init() : init({ module_or_path: module });
      await initialized;
      activateHost(host);
      return new Nanocodex(JSON.stringify(toWasmConfig({
        apiKey: apiKey ?? (mpp === undefined ? "host-managed" : "mpp-managed"),
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
