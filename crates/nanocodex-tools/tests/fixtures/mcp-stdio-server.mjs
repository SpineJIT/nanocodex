import readline from "node:readline";

const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
const toolCount = Number.parseInt(
  process.env.NANOCODEX_MCP_FIXTURE_TOOL_COUNT ?? "1",
  10,
);

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

lines.on("line", (line) => {
  const request = JSON.parse(line);
  if (request.method === "initialize") {
    send({
      jsonrpc: "2.0",
      id: request.id,
      result: {
        protocolVersion: request.params.protocolVersion,
        capabilities: { tools: {} },
        serverInfo: { name: "nanocodex-test-mcp", version: "0.1.0" },
      },
    });
  } else if (request.method === "tools/list") {
    const tools = Array.from({ length: toolCount }, (_, index) => {
      const suffix = index === 0 ? "" : `_${index}`;
      return {
        name: `echo${suffix}`,
        description: `Echo deterministic MCP fixture message ${index}.`,
        annotations: { readOnlyHint: true },
        inputSchema: {
          type: "object",
          properties: {
            message: { type: "string" },
            delay_ms: { type: "integer", minimum: 0, maximum: 1000 },
          },
          required: ["message"],
          additionalProperties: false,
        },
      };
    });
    send({
      jsonrpc: "2.0",
      id: request.id,
      result: { tools },
    });
  } else if (request.method === "tools/call") {
    const message = request.params.arguments?.message;
    const failed = message === "__fail__";
    const delayMs = request.params.arguments?.delay_ms ?? 0;
    setTimeout(() => {
      send({
        jsonrpc: "2.0",
        id: request.id,
        result: {
          content: [{
            type: "text",
            text: failed ? "fixture:synthetic failure" : `fixture:${message}`,
          }],
          structuredContent: { echoed: message },
          isError: failed,
        },
      });
    }, delayMs);
  }
});
