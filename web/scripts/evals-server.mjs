#!/usr/bin/env node

import http from "node:http";
import { promises as fs } from "node:fs";
import path from "node:path";
import { LiveEvalStore } from "./evals-data.mjs";

function parseArguments(argv) {
  const options = {
    host: process.env.NANOCODEX_EVALS_HOST ?? "127.0.0.1",
    port: Number(process.env.NANOCODEX_EVALS_PORT ?? 8788),
    intervalMs: Number(process.env.NANOCODEX_EVALS_INTERVAL_MS ?? 1_000),
    staleAfterMs: Number(process.env.NANOCODEX_EVALS_STALE_MS ?? 30_000),
    staticRoot: process.env.NANOCODEX_EVALS_STATIC ?? null,
    inputs: [],
  };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--host") options.host = argv[++index];
    else if (argument === "--port") options.port = Number(argv[++index]);
    else if (argument === "--interval-ms") options.intervalMs = Number(argv[++index]);
    else if (argument === "--stale-ms") options.staleAfterMs = Number(argv[++index]);
    else if (argument === "--static") options.staticRoot = argv[++index];
    else if (argument === "--help" || argument === "-h") options.help = true;
    else if (argument.startsWith("-")) throw new Error(`unknown argument: ${argument}`);
    else options.inputs.push(argument);
  }
  for (const [name, value] of [
    ["port", options.port],
    ["interval-ms", options.intervalMs],
    ["stale-ms", options.staleAfterMs],
  ]) {
    if (!Number.isSafeInteger(value) || value < 1) throw new Error(`--${name} must be positive`);
  }
  if (!["127.0.0.1", "::1", "localhost"].includes(options.host)) {
    throw new Error("the live eval server only binds to a loopback address");
  }
  return options;
}

function usage() {
  return `Usage: node scripts/evals-server.mjs [OPTIONS] PATH...

Streams a compact live view of retained differential sweep directories.

Options:
  --host ADDRESS       Bind address (default: 127.0.0.1)
  --port PORT          Bind port (default: 8788)
  --interval-ms MS     Snapshot interval (default: 1000)
  --stale-ms MS        Missing-heartbeat threshold (default: 30000)
  --static DIRECTORY   Also serve the standalone Evals web bundle
`;
}

const contentTypes = new Map([
  [".css", "text/css; charset=utf-8"],
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".json", "application/json; charset=utf-8"],
  [".svg", "image/svg+xml"],
]);

async function serveStatic(staticRoot, pathname, response) {
  let relative;
  try {
    relative = decodeURIComponent(pathname === "/" ? "/evals.html" : pathname);
  } catch {
    response.writeHead(400, jsonHeaders);
    response.end(JSON.stringify({ error: "invalid_path" }));
    return true;
  }
  const candidate = path.resolve(staticRoot, `.${relative}`);
  if (candidate !== staticRoot && !candidate.startsWith(`${staticRoot}${path.sep}`)) {
    response.writeHead(404, jsonHeaders);
    response.end(JSON.stringify({ error: "not_found" }));
    return true;
  }
  try {
    const body = await fs.readFile(candidate);
    response.writeHead(200, {
      "cache-control": path.extname(candidate) === ".html" ? "no-cache" : "public, max-age=31536000, immutable",
      "content-type": contentTypes.get(path.extname(candidate)) ?? "application/octet-stream",
    });
    response.end(body);
    return true;
  } catch (error) {
    if (error?.code === "ENOENT" || error?.code === "EISDIR") return false;
    response.writeHead(500, jsonHeaders);
    response.end(JSON.stringify({ error: "static_read_failed" }));
    return true;
  }
}

const jsonHeaders = {
  "cache-control": "no-store",
  "content-type": "application/json; charset=utf-8",
};

async function main() {
  const options = parseArguments(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(usage());
    return;
  }
  const store = await LiveEvalStore.open(options.inputs, {
    staleAfterMs: options.staleAfterMs,
  });
  const staticRoot = options.staticRoot ? await fs.realpath(options.staticRoot) : null;
  let snapshot = await store.snapshot();
  const clients = new Set();
  let refreshing = false;

  const publish = async () => {
    if (refreshing) return;
    refreshing = true;
    try {
      snapshot = await store.snapshot();
      const event = `event: snapshot\ndata: ${JSON.stringify(snapshot)}\n\n`;
      for (const client of clients) client.write(event);
    } catch (error) {
      process.stderr.write(`eval snapshot failed: ${error.stack ?? error}\n`);
    } finally {
      refreshing = false;
    }
  };
  const timer = setInterval(publish, options.intervalMs);
  timer.unref();

  const server = http.createServer(async (request, response) => {
    const url = new URL(request.url ?? "/", `http://${request.headers.host ?? "localhost"}`);
    if (request.method === "GET" && url.pathname === "/api/evals") {
      response.writeHead(200, jsonHeaders);
      response.end(JSON.stringify(snapshot));
      return;
    }
    if (request.method === "GET" && url.pathname === "/api/evals/events") {
      response.writeHead(200, {
        "cache-control": "no-cache, no-transform",
        connection: "keep-alive",
        "content-type": "text/event-stream; charset=utf-8",
        "x-accel-buffering": "no",
      });
      response.write(`retry: 1000\nevent: snapshot\ndata: ${JSON.stringify(snapshot)}\n\n`);
      clients.add(response);
      request.on("close", () => clients.delete(response));
      return;
    }
    if (request.method === "GET" && url.pathname === "/api/evals/health") {
      response.writeHead(200, jsonHeaders);
      response.end(JSON.stringify(snapshot.health));
      return;
    }
    if (request.method === "GET" && url.pathname === "/api/evals/case") {
      const detailId = url.searchParams.get("id");
      if (!detailId || !/^[a-f0-9]{24}$/.test(detailId)) {
        response.writeHead(400, jsonHeaders);
        response.end(JSON.stringify({ error: "invalid_case_id" }));
        return;
      }
      try {
        const detail = await store.caseDetail(detailId);
        response.writeHead(detail ? 200 : 404, jsonHeaders);
        response.end(JSON.stringify(detail ?? { error: "case_not_found" }));
      } catch (error) {
        process.stderr.write(`eval case detail failed: ${error.stack ?? error}\n`);
        response.writeHead(500, jsonHeaders);
        response.end(JSON.stringify({ error: "case_detail_failed" }));
      }
      return;
    }
    if (request.method === "GET" && staticRoot && (await serveStatic(staticRoot, url.pathname, response))) {
      return;
    }
    response.writeHead(404, jsonHeaders);
    response.end(JSON.stringify({ error: "not_found" }));
  });

  server.on("close", () => clearInterval(timer));
  server.listen(options.port, options.host, () => {
    process.stderr.write(
      `Live eval evidence: http://${options.host}:${options.port}/api/evals ` +
        `(${store.sweeps.length} sweep outputs${staticRoot ? ", web UI at /" : ""})\n`,
    );
  });
}

main().catch((error) => {
  process.stderr.write(`${error.stack ?? error}\n`);
  process.exitCode = 1;
});
