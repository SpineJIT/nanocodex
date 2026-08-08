export function createCodeRuntime(toolConfiguration = {}, extras = {}) {
  const stores = new Map();
  let nextCallId = 1;
  const definitions = [];
  const configuredTools = [];

  for (const [name, tool] of Object.entries(toolConfiguration)) {
    if (!tool || typeof tool.handler !== "function") {
      throw new TypeError(`tool ${name} requires a handler function`);
    }
    const turnBehavior = normalizeTurnBehavior(tool.turnBehavior);
    configuredTools.push(Object.freeze({
      handler: tool.handler,
      name,
      turnBehavior: turnBehavior.nested,
      hostTurnBehavior: turnBehavior.host,
    }));
    definitions.push(deepFreeze({
      type: "function",
      name,
      description: tool.description || "Application-defined tool.",
      strict: false,
      parameters: jsonSnapshot(tool.parameters || {
        type: "object",
        additionalProperties: true,
      }, `tool ${name} parameters`),
    }));
  }
  Object.freeze(configuredTools);
  Object.freeze(definitions);
  const encodedDefinitions = JSON.stringify(definitions);
  const toolByName = new Map(configuredTools.map((tool) => [tool.name, tool]));

  async function executeTool(name, encodedInput, sessionId = "default", callId = "tool") {
    const tool = toolByName.get(name);
    if (!tool) return encodeToolOutput(`unknown application tool: ${name}`, false, null);
    let input;
    try {
      input = JSON.parse(encodedInput);
    } catch (error) {
      return encodeToolOutput(`invalid tool input: ${errorMessage(error)}`, false, null);
    }
    try {
      const result = await tool.handler(input, { sessionId, parentCallId: "", callId });
      return encodeToolOutput(outputBody(result), true, clone(result) ?? null);
    } catch (error) {
      return encodeToolOutput(errorMessage(error), false, null);
    }
  }

  async function executeCode(source, sessionId = "default", parentCallId = "exec") {
    const startedAt = performance.now();
    const content = [];
    const stored = stores.get(sessionId) || new Map();
    stores.set(sessionId, stored);
    const nestedCalls = [];
    const tools = Object.create(null);
    const activeNonTerminalTools = new Set();
    let terminalBarrier = Promise.resolve();
    for (const { handler, name, turnBehavior } of configuredTools) {
      tools[name] = async (input) => {
        const callId = `${parentCallId}/code-${nextCallId++}`;
        const toolStartedAt = performance.now();
        const startedAfterNs = Math.max(
          0,
          Math.round((toolStartedAt - startedAt) * 1_000_000),
        );
        const recordedInput = clone(input) ?? null;
        try {
          const result = await executeHandler(
            handler,
            turnBehavior,
            input,
            { sessionId, parentCallId, callId },
            activeNonTerminalTools,
            () => terminalBarrier,
            (barrier) => { terminalBarrier = barrier; },
          );
          nestedCalls.push({
            call_id: callId,
            name,
            input: recordedInput,
            output: outputBody(result),
            success: true,
            turn_behavior: turnBehavior,
            started_after_ns: startedAfterNs,
            duration_ns: elapsedNs(toolStartedAt),
          });
          return result;
        } catch (error) {
          nestedCalls.push({
            call_id: callId,
            name,
            input: recordedInput,
            output: errorMessage(error),
            success: false,
            turn_behavior: turnBehavior,
            started_after_ns: startedAfterNs,
            duration_ns: elapsedNs(toolStartedAt),
          });
          throw error;
        }
      };
    }
    Object.freeze(tools);
    const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
    const EXIT = Symbol("exit");

    function text(value) {
      content.push({ type: "input_text", text: stringify(value) });
    }
    function image(value, detail = "auto") {
      if (typeof value === "string") {
        content.push({ type: "input_image", image_url: value, detail });
        return;
      }
      if (!value || typeof value !== "object" || typeof value.image_url !== "string") {
        throw new TypeError("image() requires an image URL or image item");
      }
      content.push({
        type: "input_image",
        image_url: value.image_url,
        detail: value.detail == null ? detail : value.detail,
      });
    }
    function generatedImage(result) {
      if (!result || typeof result !== "object" || typeof result.image_url !== "string") {
        throw new TypeError("generatedImage() requires an image generation result");
      }
      image(result.image_url, "high");
      if (typeof result.output_hint === "string" && result.output_hint) text(result.output_hint);
    }
    function store(key, value) {
      if (typeof key !== "string") throw new TypeError("store key must be a string");
      stored.set(key, clone(value));
    }
    function load(key) {
      return stored.has(key) ? clone(stored.get(key)) : undefined;
    }
    function exit() {
      throw EXIT;
    }

    try {
      const script = new AsyncFunction(
        "tools",
        "ALL_TOOLS",
        "text",
        "image",
        "generatedImage",
        "store",
        "load",
        "exit",
        "require",
        "console",
        source,
      );
      try {
        await script(
          tools,
          definitions,
          text,
          image,
          generatedImage,
          store,
          load,
          exit,
          extras.require,
          extras.console || console,
        );
      } catch (error) {
        if (error !== EXIT) throw error;
      }
      return JSON.stringify({
        output: withStatus("Script completed", startedAt, content),
        success: true,
        nested_calls: nestedCalls,
      });
    } catch (error) {
      return JSON.stringify({
        output: `Script failed\nWall time ${wallTime(startedAt)} seconds\nOutput:\n${errorMessage(error)}`,
        success: false,
        nested_calls: nestedCalls,
      });
    }
  }

  return Object.freeze({
    executeCode,
    executeTool,
    toolDefinitions: () => encodedDefinitions,
    toolTurnBehavior: (name) => toolByName.get(name)?.hostTurnBehavior ?? "continue",
    toolTurnBehaviors: () => JSON.stringify(Object.fromEntries(
      configuredTools.map((tool) => [tool.name, tool.hostTurnBehavior]),
    )),
    reset() {
      stores.clear();
    },
  });
}

async function executeHandler(
  handler,
  turnBehavior,
  input,
  context,
  activeNonTerminalTools,
  currentTerminalBarrier,
  setTerminalBarrier,
) {
  if (turnBehavior === "finish_turn_on_success") {
    const priorTerminal = currentTerminalBarrier();
    const priorNonTerminal = [...activeNonTerminalTools];
    let releaseTerminal;
    setTerminalBarrier(new Promise((resolve) => { releaseTerminal = resolve; }));
    await priorTerminal;
    await Promise.all(priorNonTerminal);
    try {
      return await handler(input, context);
    } finally {
      releaseTerminal();
    }
  }

  const barrier = currentTerminalBarrier();
  const execution = (async () => {
    await barrier;
    return handler(input, context);
  })();
  activeNonTerminalTools.add(execution);
  try {
    return await execution;
  } finally {
    activeNonTerminalTools.delete(execution);
  }
}

function normalizeTurnBehavior(value) {
  if (value === undefined || value === "continue") {
    return { nested: "continue", host: "continue" };
  }
  if (value === "finishTurnOnSuccess") {
    return { nested: "finish_turn_on_success", host: "finish_turn_on_success" };
  }
  if (value === "emitOutputOnSuccess") {
    return { nested: "continue", host: "emit_output_on_success" };
  }
  throw new TypeError(
    "tool turnBehavior must be continue, finishTurnOnSuccess, or emitOutputOnSuccess",
  );
}

function encodeToolOutput(output, success, codeModeValue) {
  return JSON.stringify({
    output,
    success,
    code_mode_value: codeModeValue,
    metadata: null,
    process_trace: null,
  });
}

function outputBody(value) {
  if (Array.isArray(value) && value.every((item) => item?.type === "input_text" || item?.type === "input_image")) {
    return clone(value);
  }
  return stringify(value);
}

function stringify(value) {
  if (typeof value === "string") return value;
  if (value === undefined) return "undefined";
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function clone(value) {
  if (typeof globalThis.structuredClone === "function") return structuredClone(value);
  return value === undefined ? undefined : JSON.parse(JSON.stringify(value));
}

function jsonSnapshot(value, label) {
  try {
    return JSON.parse(JSON.stringify(value));
  } catch (error) {
    throw new TypeError(`${label} must be JSON-serializable`, { cause: error });
  }
}

function deepFreeze(value) {
  if (!value || typeof value !== "object" || Object.isFrozen(value)) return value;
  for (const child of Object.values(value)) deepFreeze(child);
  return Object.freeze(value);
}

function errorMessage(error) {
  if (error && (error.stack || error.message)) return error.stack || error.message;
  return String(error);
}

function elapsedNs(startedAt) {
  return Math.max(0, Math.round((performance.now() - startedAt) * 1_000_000));
}

function wallTime(startedAt) {
  return ((performance.now() - startedAt) / 1_000).toFixed(1);
}

function withStatus(status, startedAt, content) {
  const heading = `${status}\nWall time ${wallTime(startedAt)} seconds\nOutput:\n`;
  if (!content.length) return heading;
  return [{ type: "input_text", text: heading }, ...content];
}
