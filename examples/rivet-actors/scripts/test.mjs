import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const temporaryDirectory = await mkdtemp(join(tmpdir(), "nanocodex-rivet-test-"));
const vitest = fileURLToPath(new URL("../node_modules/vitest/vitest.mjs", import.meta.url));

try {
  const exitCode = await new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [vitest, "run"], {
      env: {
        ...process.env,
        RIVET__file_system__path: join(temporaryDirectory, "engine-db"),
      },
      stdio: "inherit",
    });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (signal) reject(new Error(`vitest terminated by ${signal}`));
      else resolve(code ?? 1);
    });
  });
  process.exitCode = exitCode;
} finally {
  await rm(temporaryDirectory, { recursive: true, force: true });
}
