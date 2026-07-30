import { spawnSync } from "node:child_process";

import { registry } from "./registry.js";
import { startWebClient } from "./web-server.js";

const web = await startWebClient({
  ...(process.env.NANOCODEX_WEB_HOST === undefined
    ? {}
    : { host: process.env.NANOCODEX_WEB_HOST }),
  ...(process.env.NANOCODEX_WEB_PORT === undefined
    ? {}
    : { port: Number(process.env.NANOCODEX_WEB_PORT) }),
});
process.stderr.write(`Nanocodex browser client: ${web.url}\n`);
const stopping = shutdownSignal();
try {
  const first = await Promise.race([
    registry.startAndWait().then(() => ({ kind: "ready" as const })),
    stopping.then((signal) => ({ kind: "signal" as const, signal })),
  ]);
  const signal = first.kind === "signal" ? first.signal : await stopping;
  process.exitCode = signal === "SIGINT" ? 130 : 143;
} finally {
  await Promise.race([registry.shutdown(), delay(15_000)]);
  await web.close();
}

function shutdownSignal(): Promise<"SIGINT" | "SIGTERM"> {
  return new Promise((resolve) => {
    const received = (signal: "SIGINT" | "SIGTERM") => {
      process.removeListener("SIGINT", onInterrupt);
      process.removeListener("SIGTERM", onTerminate);
      terminateLocalEngineChildren();
      resolve(signal);
    };
    const onInterrupt = () => received("SIGINT");
    const onTerminate = () => received("SIGTERM");
    process.once("SIGINT", onInterrupt);
    process.once("SIGTERM", onTerminate);
  });
}

function terminateLocalEngineChildren(): void {
  if (process.env.RIVET_ENDPOINT || process.platform === "win32") return;
  // Rivet's local engine starts as our direct child but creates its own
  // process group. Terminate it synchronously before npm can tear down the
  // JavaScript process and orphan the engine.
  spawnSync("pkill", ["-TERM", "-P", String(process.pid)], { stdio: "ignore" });
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds).unref());
}
