import { copyFile, mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const wasmSource = fileURLToPath(import.meta.resolve("nanocodex/wasm"));
const wasmTarget = resolve(dirname(fileURLToPath(import.meta.url)), "../src/nanocodex.wasm");

await mkdir(dirname(wasmTarget), { recursive: true });
await copyFile(wasmSource, wasmTarget);
