import { readFile } from "node:fs/promises";

const lock = JSON.parse(await readFile(new URL("../package-lock.json", import.meta.url), "utf8"));
const forbidden = [
  "@agentos-software/pi",
  "@mariozechner/pi-agent-core",
  "@mariozechner/pi-ai",
  "@mariozechner/pi-coding-agent",
  "@mariozechner/pi-tui",
  "pi-acp",
];
const installed = Object.keys(lock.packages ?? {}).filter((entry) =>
  forbidden.some((name) => entry === `node_modules/${name}` || entry.endsWith(`/node_modules/${name}`))
);

if (installed.length > 0) {
  throw new Error(`forbidden Pi dependencies in package-lock.json: ${installed.join(", ")}`);
}

console.log("dependency check: no Pi packages");
