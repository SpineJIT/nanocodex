import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { LiveEvalStore, discoverSweeps } from "../scripts/evals-data.mjs";

const profile = {
  thinking: "high",
  nanocodex_tool_mode: "code_mode",
  codex_tool_mode: "code_mode",
};

async function writeJson(file, value) {
  await fs.writeFile(file, `${JSON.stringify(value)}\n`);
}

async function makeSweep(parent, name, taskName = `terminal-bench/${name}`) {
  const root = path.join(parent, name);
  await fs.mkdir(root, { recursive: true });
  await writeJson(path.join(root, "differential-sweep.json"), {
    trials: 2,
    tasks: [{ name: taskName, content_digest: `${name}-digest` }],
    profiles: [profile],
  });
  return { root, taskName };
}

function report(taskName, trial, overrides = {}) {
  const passed = () => ({
    summary: { status: "passed", outcome: "passed" },
    outcome: { attempt: { outcome: "passed", exception: null } },
    memory: { guest_peak_used_mib: 400, guest_total_mib: 626, oom_detected: false },
  });
  return {
    task: { name: taskName, content_digest: "digest" },
    trial,
    thinking: "high",
    policy: { nanocodex_tool_mode: "code_mode", codex_tool_mode: "code_mode" },
    schedule: {
      infrastructure_replacement_for: null,
      max_infrastructure_replacements: 10,
      memory_attempt: 1,
    },
    classification: "both_passed",
    finished_at: new Date().toISOString(),
    duration_ms: 10_000,
    nanocodex: passed(),
    codex: passed(),
    artifacts: {},
    ...overrides,
  };
}

async function writeAttempt(root, directory, value) {
  const attempt = path.join(root, directory);
  await fs.mkdir(attempt, { recursive: true });
  await writeJson(path.join(attempt, "comparison.json"), value);
}

test("discovers split sweeps and folds replacement evidence into stable cells", async (context) => {
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), "nanocodex-live-evals-"));
  context.after(() => fs.rm(temporary, { recursive: true, force: true }));
  const first = await makeSweep(temporary, "first");
  const second = await makeSweep(temporary, "second");

  const taskRoot = path.join(temporary, "task-source");
  await fs.mkdir(taskRoot);
  await fs.writeFile(
    path.join(taskRoot, "instruction.md"),
    "Repair the retained fixture on dev-example.tailtest.ts.net from /home/example/task.\n",
  );
  const firstDirectory = "first__high__nanocodex_code_mode__codex_code_mode__001__valid";
  const verifierDirectory = path.join(first.root, firstDirectory, "nanocodex", "verifier");
  await fs.mkdir(verifierDirectory, { recursive: true });
  const verifierOutput = path.join(verifierDirectory, "test-stdout.txt");
  await fs.writeFile(
    verifierOutput,
    "fixture verifier output from /home/example/build via 192.0.2.10:8080\n",
  );
  const firstReport = report(first.taskName, 1);
  firstReport.task.root = taskRoot;
  firstReport.nanocodex.outcome.attempt.agent = {
    final_message: "fixture final message with Bearer very-secret-token",
  };
  firstReport.nanocodex.outcome.attempt.artifacts = { verifier_output: verifierOutput };
  firstReport.codex.outcome.attempt.artifacts = {
    verifier_output: path.join(taskRoot, "instruction.md"),
  };
  firstReport.codex.outcome.attempt.exception = {
    kind: "agent",
    outcome: "infrastructure_error",
    message: "connect 198.51.100.20:9090 through /mnt/eval-private/socket",
  };
  await writeAttempt(
    first.root,
    firstDirectory,
    firstReport,
  );
  const failedArm = {
    summary: { status: "errored", outcome: "infrastructure_error" },
    outcome: {
      attempt: {
        outcome: "infrastructure_error",
        exception: { message: "guest launch failed" },
      },
    },
    memory: { guest_peak_used_mib: 0, guest_total_mib: 626, oom_detected: false },
  };
  await writeAttempt(
    second.root,
    "second__high__nanocodex_code_mode__codex_code_mode__001__failed",
    report(second.taskName, 1, {
      classification: "incomplete",
      nanocodex: failedArm,
      codex: failedArm,
    }),
  );
  await writeAttempt(
    second.root,
    "second__high__nanocodex_code_mode__codex_code_mode__003__replacement",
    report(second.taskName, 3, {
      schedule: {
        infrastructure_replacement_for: 1,
        max_infrastructure_replacements: 10,
        memory_attempt: 1,
      },
    }),
  );

  const active = path.join(
    first.root,
    "first__high__nanocodex_code_mode__codex_code_mode__002__active",
  );
  await fs.mkdir(active);
  await fs.writeFile(path.join(active, "rootfs.upper.ext4"), "fixture");
  await fs.writeFile(
    path.join(active, "progress.jsonl"),
    `${JSON.stringify({
      schema_version: 1,
      sequence: 1,
      observed_at: new Date().toISOString(),
      elapsed_ms: 5_000,
      arm: "nanocodex",
      kind: "api.response.started",
      summary: "model response",
    })}\n`,
  );

  const tombstone = path.join(
    first.root,
    "first__high__nanocodex_code_mode__codex_code_mode__004__tombstone",
  );
  await fs.mkdir(tombstone);
  await fs.writeFile(
    path.join(tombstone, "progress.jsonl"),
    `${JSON.stringify({
      observed_at: new Date(Date.now() - 60_000).toISOString(),
      elapsed_ms: 1_000,
      arm: "runner",
      kind: "heartbeat",
      summary: "old restart",
    })}\n`,
  );

  const sweeps = await discoverSweeps([temporary]);
  assert.equal(sweeps.length, 2);
  const store = new LiveEvalStore(sweeps, { staleAfterMs: 30_000 });
  const snapshot = await store.snapshot();

  assert.deepEqual(
    snapshot.rows.map((row) => [row.taskLabel, row.cells.map((cell) => cell.state)]),
    [
      ["first", ["completed", "running"]],
      ["second", ["completed", "queued"]],
    ],
  );
  assert.equal(snapshot.rows[1].cells[0].attempts, 2);
  assert.equal(snapshot.rows[1].cells[0].classification, "both_passed");
  assert.equal(snapshot.rows[0].cells[1].nanocodexPhase.summary, "Model activity observed");
  assert.doesNotMatch(JSON.stringify(snapshot), /guest launch failed|model response/);
  assert.deepEqual(
    {
      total: snapshot.summary.total,
      completed: snapshot.summary.completed,
      running: snapshot.summary.running,
      retrying: snapshot.summary.retrying,
      queued: snapshot.summary.queued,
      stale: snapshot.summary.stale,
    },
    { total: 4, completed: 2, running: 1, retrying: 0, queued: 1, stale: 0 },
  );

  const firstDetail = await store.caseDetail(snapshot.rows[0].cells[0].detailId);
  assert.equal(
    firstDetail.task.instruction.text,
    "Repair the retained fixture on [tailnet-host] from [host-path]\n",
  );
  assert.equal(
    firstDetail.nanocodex.verifier.stdout.text,
    "fixture verifier output from [host-path] via [network-address]\n",
  );
  assert.equal(firstDetail.nanocodex.finalMessage, "fixture final message with Bearer [redacted]");
  assert.equal(
    firstDetail.codex.exception.message,
    "connect [network-address] through [host-path]",
  );
  assert.equal(firstDetail.codex.verifier.stdout, null);
  assert.doesNotMatch(
    JSON.stringify(firstDetail),
    /dev-example|tailtest|home\/example|mnt\/eval-private|192\.0\.2\.10|198\.51\.100\.20|very-secret-token/,
  );
  const replacementDetail = await store.caseDetail(snapshot.rows[1].cells[0].detailId);
  assert.equal(replacementDetail.requestedTrial, 1);
  assert.equal(replacementDetail.actualTrial, 3);
  assert.deepEqual(replacementDetail.history.map((attempt) => attempt.trial), [1, 3]);
  assert.doesNotMatch(
    JSON.stringify(firstDetail),
    /taskToml|hostPeakRssMib|guestPeakUsedMib|guestTotalMib|memoryAttempt/,
  );
  assert.equal(await store.caseDetail("000000000000000000000000"), null);
  assert.deepEqual(Object.keys(snapshot.health.host).sort(), [
    "cpuPercent",
    "diskUsedPercent",
    "loadPercent",
    "memoryUsedPercent",
    "swapUsedPercent",
  ]);
  assert.doesNotMatch(
    JSON.stringify(snapshot.health.host),
    /logicalCpus|totalBytes|availableBytes|swapTotalBytes|diskTotalBytes|load1/,
  );
});

test("reconsiders a stale startup tombstone when append-only evidence resumes", async (context) => {
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), "nanocodex-live-evals-"));
  context.after(() => fs.rm(temporary, { recursive: true, force: true }));
  const sweep = await makeSweep(temporary, "recovering");
  const attempt = path.join(
    sweep.root,
    "recovering__high__nanocodex_code_mode__codex_code_mode__001__active",
  );
  await fs.mkdir(attempt);
  const progressPath = path.join(attempt, "progress.jsonl");
  await fs.writeFile(
    progressPath,
    `${JSON.stringify({
      observed_at: new Date(Date.now() - 60_000).toISOString(),
      elapsed_ms: 1_000,
      arm: "runner",
      kind: "comparison.started",
    })}\n`,
  );
  const [discovered] = await discoverSweeps([sweep.root]);
  const canonicalAttempt = path.join(discovered.root, path.basename(attempt));
  const canonicalProgressPath = path.join(canonicalAttempt, "progress.jsonl");
  const store = new LiveEvalStore([discovered], { staleAfterMs: 30_000 });

  const stale = await store.snapshot();
  assert.equal(stale.rows[0].cells[0].state, "queued");
  assert.equal(store.tombstones.has(canonicalAttempt), true);
  const cached = store.progressCache.get(canonicalProgressPath);

  await fs.writeFile(path.join(attempt, "rootfs.upper.ext4"), "fixture");
  await fs.appendFile(
    progressPath,
    `${JSON.stringify({
      observed_at: new Date().toISOString(),
      elapsed_ms: 61_000,
      arm: "nanocodex",
      kind: "attempt.started",
    })}\n`,
  );
  const resumed = await store.snapshot();

  assert.equal(resumed.rows[0].cells[0].state, "running");
  assert.equal(store.tombstones.has(canonicalAttempt), false);
  assert.equal(store.progressCache.get(canonicalProgressPath), cached);
  assert.equal(
    store.progressCache.get(canonicalProgressPath).offset,
    (await fs.stat(progressPath)).size,
  );
});

test("structured progress identity distinguishes tasks with the same short name", async (context) => {
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), "nanocodex-live-evals-"));
  context.after(() => fs.rm(temporary, { recursive: true, force: true }));
  const root = path.join(temporary, "colliding");
  await fs.mkdir(root);
  await writeJson(path.join(root, "differential-sweep.json"), {
    trials: 1,
    tasks: [
      { name: "suite-a/install", content_digest: "digest-a" },
      { name: "suite-b/install", content_digest: "digest-b" },
    ],
    profiles: [profile],
  });
  const attempt = path.join(
    root,
    "install__high__nanocodex_code_mode__codex_code_mode__001__active",
  );
  await fs.mkdir(attempt);
  await fs.writeFile(path.join(attempt, "rootfs.upper.ext4"), "fixture");
  await fs.writeFile(
    path.join(attempt, "progress.jsonl"),
    `${JSON.stringify({
      observed_at: new Date().toISOString(),
      elapsed_ms: 5_000,
      arm: "runner",
      kind: "comparison.started",
      coordinate: {
        task_name: "suite-b/install",
        task_content_digest: "digest-b",
        thinking: "high",
        nanocodex_tool_mode: "code_mode",
        codex_tool_mode: "code_mode",
        trial: 1,
      },
    })}\n`,
  );

  const snapshot = await new LiveEvalStore(await discoverSweeps([root])).snapshot();
  const states = Object.fromEntries(
    snapshot.rows.map((row) => [row.taskName, row.cells[0].state]),
  );
  assert.deepEqual(states, {
    "suite-a/install": "queued",
    "suite-b/install": "running",
  });
});
