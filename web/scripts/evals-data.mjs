import { createHash } from "node:crypto";
import { promises as fs } from "node:fs";
import os from "node:os";
import path from "node:path";
import { redactPublicText } from "./public-redaction.mjs";

const MANIFEST = "differential-sweep.json";
const REPORT = "comparison.json";
const PROGRESS = "progress.jsonl";
const MAX_INSTRUCTION_BYTES = 128 * 1024;
const MAX_VERIFIER_BYTES = 256 * 1024;
const MAX_MESSAGE_CHARS = 32 * 1024;

const asArray = (value) => (Array.isArray(value) ? value : []);
const asObject = (value) =>
  value && typeof value === "object" && !Array.isArray(value) ? value : {};
const text = (value) => (typeof value === "string" ? value : null);
const number = (value) => (typeof value === "number" && Number.isFinite(value) ? value : null);

function profileKey(thinking, nanocodex, codex) {
  return `${thinking}\0${nanocodex}\0${codex}`;
}

function coordinateKey(task, profile) {
  return `${task}\0${profile}`;
}

function shortTaskName(name) {
  return name.includes("/") ? name.slice(name.lastIndexOf("/") + 1) : name;
}

function profileLabel(profile) {
  if (profile.nanocodexToolMode === profile.codexToolMode) {
    return profile.nanocodexToolMode === "code_mode" ? "CM" : "CMO";
  }
  return `N:${profile.nanocodexToolMode} / C:${profile.codexToolMode}`;
}

async function pathType(candidate) {
  try {
    const stat = await fs.stat(candidate);
    return stat.isDirectory() ? "directory" : stat.isFile() ? "file" : "other";
  } catch (error) {
    if (error?.code === "ENOENT") return "missing";
    throw error;
  }
}

async function findManifests(input) {
  const type = await pathType(input);
  if (type === "missing") throw new Error(`eval input does not exist: ${input}`);
  if (type === "file") {
    if (path.basename(input) !== MANIFEST) {
      throw new Error(`eval input file must be ${MANIFEST}: ${input}`);
    }
    return [path.resolve(input)];
  }
  if (type !== "directory") return [];

  const root = path.resolve(input);
  const direct = path.join(root, MANIFEST);
  if ((await pathType(direct)) === "file") return [direct];

  const manifests = [];
  const pending = [{ directory: root, depth: 0 }];
  while (pending.length) {
    const { directory, depth } = pending.pop();
    if (depth > 5) continue;
    for (const entry of await fs.readdir(directory, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      if (entry.name.startsWith(".") || entry.name === "assets") continue;
      const child = path.join(directory, entry.name);
      const manifest = path.join(child, MANIFEST);
      if ((await pathType(manifest)) === "file") manifests.push(manifest);
      else pending.push({ directory: child, depth: depth + 1 });
    }
  }
  return manifests;
}

export async function discoverSweeps(inputs) {
  if (!inputs.length) throw new Error("at least one eval output or run directory is required");
  const manifests = new Set();
  for (const input of inputs) {
    for (const manifest of await findManifests(input)) manifests.add(await fs.realpath(manifest));
  }
  if (!manifests.size) throw new Error(`no ${MANIFEST} files found beneath the supplied inputs`);

  const sweeps = [];
  for (const manifestPath of [...manifests].sort()) {
    const manifest = JSON.parse(await fs.readFile(manifestPath, "utf8"));
    const trials = number(manifest.trials);
    if (!trials || trials < 1) throw new Error(`invalid trial count in ${manifestPath}`);
    const tasks = asArray(manifest.tasks).map((task) => ({
      name: String(task.name),
      contentDigest: String(task.content_digest),
      root: text(task.root),
    }));
    const profiles = asArray(manifest.profiles).map((profile) => ({
      thinking: String(profile.thinking),
      nanocodexToolMode: String(profile.nanocodex_tool_mode),
      codexToolMode: String(profile.codex_tool_mode),
    }));
    if (!tasks.length || !profiles.length) throw new Error(`empty sweep manifest ${manifestPath}`);
    sweeps.push({ root: path.dirname(manifestPath), manifestPath, trials, tasks, profiles });
  }
  return sweeps;
}

function armOutcome(arm) {
  const summary = asObject(arm.summary);
  const attempt = asObject(asObject(arm.outcome).attempt);
  return {
    status: text(summary.status) ?? "unknown",
    outcome: text(summary.outcome) ?? text(attempt.outcome) ?? "unknown",
    error: text(asObject(attempt.exception).message),
    memoryUsedMib: number(asObject(arm.memory).guest_peak_used_mib),
    memoryTotalMib: number(asObject(arm.memory).guest_total_mib),
    oom: asObject(arm.memory).oom_detected === true,
  };
}

function parseReport(raw, reportPath) {
  const task = asObject(raw.task);
  const policy = asObject(raw.policy);
  const schedule = asObject(raw.schedule);
  const artifacts = asObject(raw.artifacts);
  const nanocodex = armOutcome(asObject(raw.nanocodex));
  const codex = armOutcome(asObject(raw.codex));
  const operational = Boolean(
    artifacts.progress_error ||
      artifacts.api_comparison_error ||
      artifacts.profile_validation_error ||
      asObject(raw.nanocodex).operational_error ||
      asObject(raw.nanocodex).event_error ||
      asObject(raw.nanocodex).trajectory_error ||
      asObject(raw.nanocodex).api_capture_error ||
      asObject(raw.codex).operational_error ||
      asObject(raw.codex).event_error ||
      asObject(raw.codex).trajectory_error ||
      asObject(raw.codex).api_capture_error,
  );
  const infrastructure =
    nanocodex.outcome === "infrastructure_error" ||
    codex.outcome === "infrastructure_error" ||
    nanocodex.oom ||
    codex.oom;
  const classification = text(raw.classification) ?? "incomplete";
  return {
    detailId: createHash("sha256").update(reportPath).digest("hex").slice(0, 24),
    reportPath,
    taskName: String(task.name),
    taskDigest: String(task.content_digest),
    trial: Number(raw.trial),
    thinking: String(raw.thinking),
    nanocodexToolMode: String(policy.nanocodex_tool_mode),
    codexToolMode: String(policy.codex_tool_mode),
    profile: profileKey(raw.thinking, policy.nanocodex_tool_mode, policy.codex_tool_mode),
    classification,
    replacementFor: number(schedule.infrastructure_replacement_for),
    maxReplacements: number(schedule.max_infrastructure_replacements) ?? 0,
    memoryAttempt: number(schedule.memory_attempt) ?? 1,
    finishedAt: text(raw.finished_at),
    finishedMs: Date.parse(raw.finished_at) || 0,
    durationMs: number(raw.duration_ms),
    nanocodex,
    codex,
    infrastructure,
    operational,
    valid: classification !== "incomplete" && !infrastructure && !operational,
    error:
      nanocodex.error ??
      codex.error ??
      text(artifacts.profile_validation_error) ??
      text(artifacts.progress_error) ??
      text(artifacts.api_comparison_error),
  };
}

function parseDirectoryCoordinate(directoryName, sweep) {
  const parts = directoryName.split("__");
  if (parts.length < 6) return null;
  const [shortName, thinking, nanocodexPart, codexPart, trialPart] = parts;
  const task = sweep.tasks.find((candidate) => shortTaskName(candidate.name) === shortName);
  if (!task) return null;
  const nanocodexToolMode = nanocodexPart.replace(/^nanocodex_/, "");
  const codexToolMode = codexPart.replace(/^codex_/, "");
  const trial = Number.parseInt(trialPart, 10);
  if (!Number.isSafeInteger(trial) || trial < 1) return null;
  return {
    taskName: task.name,
    taskDigest: task.contentDigest,
    thinking,
    nanocodexToolMode,
    codexToolMode,
    profile: profileKey(thinking, nanocodexToolMode, codexToolMode),
    trial,
  };
}

function parseProgress(raw) {
  return {
    observedAt: text(raw.observed_at),
    observedMs: Date.parse(raw.observed_at) || 0,
    elapsedMs: number(raw.elapsed_ms) ?? 0,
    arm: text(raw.arm) ?? "runner",
    kind: text(raw.kind) ?? "unknown",
    summary: text(raw.summary),
  };
}

async function readProgress(progressPath) {
  let contents;
  try {
    contents = await fs.readFile(progressPath, "utf8");
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw error;
  }
  const latestByArm = {};
  let latest = null;
  for (const line of contents.split("\n")) {
    if (!line) continue;
    try {
      const progress = parseProgress(JSON.parse(line));
      if (!latest || progress.observedMs >= latest.observedMs) latest = progress;
      if (["nanocodex", "codex"].includes(progress.arm)) latestByArm[progress.arm] = progress;
    } catch {
      // The writer may be in the middle of appending the final line.
    }
  }
  return latest ? { latest, latestByArm } : null;
}

async function containsOverlay(directory) {
  const pending = [directory];
  while (pending.length) {
    const current = pending.pop();
    let entries;
    try {
      entries = await fs.readdir(current, { withFileTypes: true });
    } catch (error) {
      if (error?.code === "ENOENT") continue;
      throw error;
    }
    for (const entry of entries) {
      if (entry.isFile() && ["rootfs.upper.ext4", "cache.ext4"].includes(entry.name)) return true;
      if (entry.isDirectory()) pending.push(path.join(current, entry.name));
    }
  }
  return false;
}

function latestReportsByTrial(reports) {
  const latest = new Map();
  for (const report of reports) {
    const previous = latest.get(report.trial);
    if (
      !previous ||
      report.memoryAttempt > previous.memoryAttempt ||
      (report.memoryAttempt === previous.memoryAttempt && report.finishedMs > previous.finishedMs)
    ) {
      latest.set(report.trial, report);
    }
  }
  return latest;
}

function lineageRoot(trial, reports, requestedTrials) {
  const seen = new Set();
  let current = trial;
  while (current > requestedTrials && !seen.has(current)) {
    seen.add(current);
    const parent = reports.get(current)?.replacementFor;
    if (!parent) break;
    current = parent;
  }
  return current <= requestedTrials ? current : null;
}

function phaseView(progress) {
  if (!progress) return null;
  let phase = "working";
  let summary = "Agent work in progress";
  if (progress.kind.startsWith("api.")) phase = "model";
  if (phase === "model") summary = "Model activity observed";
  else if (progress.kind.includes("command") || progress.kind.includes("tool")) {
    phase = "tool";
    summary = "Tool activity observed";
  } else if (progress.kind.includes("verifier")) {
    phase = "verifier";
    summary = "Verifier activity observed";
  } else if (progress.kind.includes("environment") || progress.kind.includes("vm")) {
    phase = "environment";
    summary = "Environment activity observed";
  } else if (progress.kind.includes("attempt.completed")) {
    phase = "completed";
    summary = "Attempt completed";
  } else if (progress.kind.includes("attempt.failed")) {
    phase = "failed";
    summary = "Attempt failed";
  }
  return {
    phase,
    kind: progress.kind,
    summary,
    elapsedMs: progress.elapsedMs,
    observedAt: progress.observedAt,
  };
}

function completedArmState(arm) {
  if (arm.status === "passed") return "passed";
  if (arm.outcome === "infrastructure_error") return "error";
  if (arm.status === "unknown") return "unknown";
  return "failed";
}

function boundedString(value, maxChars = MAX_MESSAGE_CHARS) {
  const content = text(value);
  if (!content) return null;
  const bounded = content.length > maxChars ? `${content.slice(0, maxChars)}\n… truncated` : content;
  return redactPublicText(bounded);
}

function sanitizeEvidence(evidence) {
  return evidence ? { ...evidence, text: redactPublicText(evidence.text) } : null;
}

async function readBoundedFile(file, maxBytes) {
  let handle;
  try {
    handle = await fs.open(file, "r");
    const stat = await handle.stat();
    if (!stat.isFile()) return null;
    const bytes = Math.min(stat.size, maxBytes + 1);
    const buffer = Buffer.alloc(bytes);
    const { bytesRead } = await handle.read(buffer, 0, bytes, 0);
    const truncated = stat.size > maxBytes;
    return {
      text: buffer.subarray(0, Math.min(bytesRead, maxBytes)).toString("utf8"),
      truncated,
    };
  } catch (error) {
    if (["ENOENT", "EACCES"].includes(error?.code)) return null;
    throw error;
  } finally {
    await handle?.close();
  }
}

async function readKnownFile(root, file, maxBytes) {
  if (!root || !file) return null;
  try {
    const [realRoot, realFile] = await Promise.all([fs.realpath(root), fs.realpath(file)]);
    if (realFile !== realRoot && !realFile.startsWith(`${realRoot}${path.sep}`)) return null;
    return await readBoundedFile(realFile, maxBytes);
  } catch (error) {
    if (["ENOENT", "EACCES"].includes(error?.code)) return null;
    throw error;
  }
}

function detailUsage(summary) {
  const usage = asObject(summary.usage);
  return {
    inputTokens: number(usage.input_tokens) ?? 0,
    cachedInputTokens: number(usage.cached_input_tokens) ?? 0,
    outputTokens: number(usage.output_tokens) ?? 0,
    reasoningOutputTokens: number(usage.reasoning_output_tokens) ?? 0,
    totalTokens: number(usage.total_tokens) ?? 0,
  };
}

async function detailArm(rawArm, reportDirectory) {
  const arm = asObject(rawArm);
  const summary = asObject(arm.summary);
  const attempt = asObject(asObject(arm.outcome).attempt);
  const exception = asObject(attempt.exception);
  const verifier = asObject(attempt.verifier);
  const artifacts = asObject(attempt.artifacts);
  const output = text(artifacts.verifier_output);
  const stdout = sanitizeEvidence(
    await readKnownFile(reportDirectory, output, MAX_VERIFIER_BYTES),
  );
  const stderr = output
    ? sanitizeEvidence(
        await readKnownFile(
          reportDirectory,
          path.join(path.dirname(output), "test-stderr.txt"),
          MAX_VERIFIER_BYTES,
        ),
      )
    : null;
  const memory = asObject(arm.memory);
  const guestPeakUsedMib = number(memory.guest_peak_used_mib);
  const guestTotalMib = number(memory.guest_total_mib);
  return {
    status: text(summary.status) ?? text(attempt.status) ?? "unknown",
    outcome: text(summary.outcome) ?? text(attempt.outcome) ?? "unknown",
    model: text(summary.model),
    durationMs: number(summary.duration_ms),
    toolCalls: number(summary.tool_calls),
    usage: detailUsage(summary),
    memory: {
      guestUsedPercent:
        guestPeakUsedMib !== null && guestTotalMib
          ? (guestPeakUsedMib / guestTotalMib) * 100
          : null,
      oomDetected: memory.oom_detected === true,
    },
    exception: Object.keys(exception).length
      ? {
          kind: text(exception.kind),
          outcome: text(exception.outcome),
          message: boundedString(exception.message),
          occurredAt: text(exception.occurred_at),
        }
      : null,
    verifier: {
      exitCode: number(verifier.exit_code) ?? number(summary.verifier_exit_code),
      rewards: asObject(verifier.rewards),
      stdout,
      stderr,
    },
    finalMessage: boundedString(asObject(attempt.agent).final_message),
  };
}

function buildRows(catalog, evidence, nowMs) {
  const rows = [];
  const recentFailures = [];
  const counters = {
    total: 0,
    completed: 0,
    running: 0,
    retrying: 0,
    queued: 0,
    stale: 0,
    bothPassed: 0,
    nanocodexOnly: 0,
    codexOnly: 0,
    neitherPassed: 0,
  };
  let latestEvidenceMs = 0;
  let reports5m = 0;
  let reports15m = 0;
  let infrastructure15m = 0;

  for (const entry of [...catalog.values()].sort((left, right) =>
    left.taskName.localeCompare(right.taskName) || left.order - right.order)) {
    const bucket = evidence.get(entry.key) ?? { reports: [], active: [] };
    const latest = latestReportsByTrial(bucket.reports);
    const linkedParents = new Set([...latest.values()].map((report) => report.replacementFor).filter(Boolean));
    const cells = Array.from({ length: entry.trials }, (_, index) => ({
      trial: index + 1,
      reports: [],
      active: [],
    }));

    for (const report of latest.values()) {
      const root = lineageRoot(report.trial, latest, entry.trials);
      if (root) cells[root - 1].reports.push(report);
      latestEvidenceMs = Math.max(latestEvidenceMs, report.finishedMs);
      if (nowMs - report.finishedMs <= 5 * 60_000) reports5m += 1;
      if (nowMs - report.finishedMs <= 15 * 60_000) {
        reports15m += 1;
        if (report.infrastructure || report.operational) infrastructure15m += 1;
      }
      if (!report.valid) recentFailures.push(report);
    }

    const unlinkedFailure = [...latest.values()]
      .filter((report) => !report.valid && !linkedParents.has(report.trial))
      .sort((left, right) => right.trial - left.trial)[0];
    for (const active of bucket.active) {
      const root =
        active.trial <= entry.trials
          ? active.trial
          : unlinkedFailure
            ? lineageRoot(unlinkedFailure.trial, latest, entry.trials)
            : null;
      if (root) cells[root - 1].active.push(active);
      latestEvidenceMs = Math.max(latestEvidenceMs, active.progress.latest.observedMs);
    }

    const renderedCells = cells.map((cell) => {
      counters.total += 1;
      const valid = cell.reports.filter((report) => report.valid).sort((a, b) => b.finishedMs - a.finishedMs)[0];
      const newest = [...cell.reports].sort((a, b) => b.finishedMs - a.finishedMs)[0];
      const active = [...cell.active].sort(
        (a, b) => b.progress.latest.observedMs - a.progress.latest.observedMs,
      )[0];
      const attempts = cell.reports.length + cell.active.length;

      if (active) {
        const state = active.stale ? "stale" : "running";
        counters[state] += 1;
        return {
          trial: cell.trial,
          state,
          attempts,
          classification: newest?.classification ?? null,
          nanocodex: active.progress.latestByArm.nanocodex
            ? "running"
            : newest
              ? completedArmState(newest.nanocodex)
              : "queued",
          codex: active.progress.latestByArm.codex
            ? "running"
            : newest
              ? completedArmState(newest.codex)
              : "queued",
          nanocodexPhase: phaseView(active.progress.latestByArm.nanocodex),
          codexPhase: phaseView(active.progress.latestByArm.codex),
          updatedAt: active.progress.latest.observedAt,
          durationMs: active.progress.latest.elapsedMs,
          message: active.stale ? "No progress heartbeat within the stale threshold" : null,
          detailId: newest?.detailId ?? null,
        };
      }

      if (valid) {
        counters.completed += 1;
        if (valid.classification === "both_passed") counters.bothPassed += 1;
        else if (valid.classification === "nanocodex_only_passed") counters.nanocodexOnly += 1;
        else if (valid.classification === "codex_only_passed") counters.codexOnly += 1;
        else if (valid.classification === "neither_passed") counters.neitherPassed += 1;
        return {
          trial: cell.trial,
          state: "completed",
          attempts,
          classification: valid.classification,
          nanocodex: completedArmState(valid.nanocodex),
          codex: completedArmState(valid.codex),
          nanocodexPhase: null,
          codexPhase: null,
          updatedAt: valid.finishedAt,
          durationMs: valid.durationMs,
          message: null,
          detailId: valid.detailId,
        };
      }

      if (newest) {
        counters.retrying += 1;
        return {
          trial: cell.trial,
          state: "retrying",
          attempts,
          classification: newest.classification,
          nanocodex: completedArmState(newest.nanocodex),
          codex: completedArmState(newest.codex),
          nanocodexPhase: null,
          codexPhase: null,
          updatedAt: newest.finishedAt,
          durationMs: newest.durationMs,
          message: "Infrastructure or evidence failure; waiting for replacement",
          detailId: newest.detailId,
        };
      }

      counters.queued += 1;
      return {
        trial: cell.trial,
        state: "queued",
        attempts: 0,
        classification: null,
        nanocodex: "queued",
        codex: "queued",
        nanocodexPhase: null,
        codexPhase: null,
        updatedAt: null,
        durationMs: null,
        message: null,
        detailId: null,
      };
    });

    rows.push({
      taskName: entry.taskName,
      taskLabel: shortTaskName(entry.taskName),
      taskDigest: entry.taskDigest,
      profile: entry.profile,
      profileLabel: profileLabel(entry),
      thinking: entry.thinking,
      nanocodexToolMode: entry.nanocodexToolMode,
      codexToolMode: entry.codexToolMode,
      cells: renderedCells,
    });
  }

  recentFailures.sort((left, right) => right.finishedMs - left.finishedMs);
  return {
    rows,
    counters,
    latestEvidenceMs,
    reports5m,
    reports15m,
    infrastructure15m,
    recentFailures: recentFailures.slice(0, 12).map((report) => ({
      taskName: report.taskName,
      trial: report.trial,
      profileLabel: report.nanocodexToolMode === "code_mode" ? "CM" : "CMO",
      classification: report.classification,
      nanocodex: report.nanocodex.outcome,
      codex: report.codex.outcome,
      message: report.error ? "Retained failure detail is available on the eval host" : null,
      finishedAt: report.finishedAt,
    })),
  };
}

async function readCpuSample() {
  const line = (await fs.readFile("/proc/stat", "utf8")).split("\n")[0];
  const values = line.trim().split(/\s+/).slice(1).map(Number);
  const idle = (values[3] ?? 0) + (values[4] ?? 0);
  const total = values.reduce((sum, value) => sum + value, 0);
  return { idle, total };
}

async function readMemory() {
  const values = {};
  for (const line of (await fs.readFile("/proc/meminfo", "utf8")).split("\n")) {
    const match = /^(\w+):\s+(\d+) kB$/.exec(line);
    if (match) values[match[1]] = Number(match[2]) * 1024;
  }
  return {
    totalBytes: values.MemTotal ?? os.totalmem(),
    availableBytes: values.MemAvailable ?? os.freemem(),
    swapTotalBytes: values.SwapTotal ?? 0,
    swapUsedBytes: Math.max(0, (values.SwapTotal ?? 0) - (values.SwapFree ?? 0)),
  };
}

export class LiveEvalStore {
  constructor(sweeps, { staleAfterMs = 30_000 } = {}) {
    this.sweeps = sweeps;
    this.staleAfterMs = staleAfterMs;
    this.reportCache = new Map();
    this.detailIndex = new Map();
    this.tombstones = new Set();
    this.cpuSample = null;
    this.sequence = 0;
  }

  static async open(inputs, options) {
    return new LiveEvalStore(await discoverSweeps(inputs), options);
  }

  async hostHealth() {
    let cpuPercent = null;
    try {
      const sample = await readCpuSample();
      if (this.cpuSample) {
        const total = sample.total - this.cpuSample.total;
        const idle = sample.idle - this.cpuSample.idle;
        if (total > 0) cpuPercent = Math.max(0, Math.min(100, ((total - idle) / total) * 100));
      }
      this.cpuSample = sample;
    } catch {
      // Non-Linux development keeps the rest of the dashboard useful.
    }
    const memory = await readMemory().catch(() => ({
      totalBytes: os.totalmem(),
      availableBytes: os.freemem(),
      swapTotalBytes: 0,
      swapUsedBytes: 0,
    }));
    const disk = await fs.statfs(this.sweeps[0].root).catch(() => null);
    return {
      logicalCpus: os.cpus().length,
      cpuPercent,
      load1: os.loadavg()[0],
      ...memory,
      diskTotalBytes: disk ? Number(disk.blocks) * Number(disk.bsize) : null,
      diskAvailableBytes: disk ? Number(disk.bavail) * Number(disk.bsize) : null,
    };
  }

  async caseDetail(detailId) {
    const selected = this.detailIndex.get(detailId);
    if (!selected) return null;
    const raw = JSON.parse(await fs.readFile(selected.reportPath, "utf8"));
    const task = asObject(raw.task);
    const policy = asObject(raw.policy);
    const related = [...this.reportCache.values()].filter(
      (report) => report.taskName === selected.taskName && report.profile === selected.profile,
    );
    const latest = latestReportsByTrial(related);
    const sweep = this.sweeps.find((candidate) =>
      candidate.tasks.some((entry) => entry.name === selected.taskName),
    );
    const requestedTrials = sweep?.trials ?? selected.trial;
    const requestedTrial = lineageRoot(selected.trial, latest, requestedTrials) ?? selected.trial;
    const history = related
      .filter((report) => lineageRoot(report.trial, latest, requestedTrials) === requestedTrial)
      .sort(
        (left, right) =>
          left.trial - right.trial ||
          left.memoryAttempt - right.memoryAttempt ||
          left.finishedMs - right.finishedMs,
      )
      .map((report) => ({
        trial: report.trial,
        replacementFor: report.replacementFor,
        classification: report.classification,
        nanocodexOutcome: report.nanocodex.outcome,
        codexOutcome: report.codex.outcome,
        finishedAt: report.finishedAt,
        durationMs: report.durationMs,
      }));
    const reportDirectory = path.dirname(selected.reportPath);
    const taskRoot = text(task.root);
    const [instruction, nanocodex, codex] = await Promise.all([
      readKnownFile(taskRoot, taskRoot ? path.join(taskRoot, "instruction.md") : null, MAX_INSTRUCTION_BYTES),
      detailArm(raw.nanocodex, reportDirectory),
      detailArm(raw.codex, reportDirectory),
    ]);
    return {
      schemaVersion: 1,
      detailId,
      task: {
        name: String(task.name),
        contentDigest: String(task.content_digest),
        instruction: sanitizeEvidence(instruction),
      },
      requestedTrial,
      actualTrial: Number(raw.trial),
      thinking: String(raw.thinking),
      profileLabel:
        policy.nanocodex_tool_mode === "code_mode" && policy.codex_tool_mode === "code_mode"
          ? "CM"
          : "CMO",
      classification: text(raw.classification) ?? "incomplete",
      finishedAt: text(raw.finished_at),
      durationMs: number(raw.duration_ms),
      history,
      nanocodex,
      codex,
    };
  }

  async snapshot() {
    const nowMs = Date.now();
    const catalog = new Map();
    const evidence = new Map();

    for (const sweep of this.sweeps) {
      for (const task of sweep.tasks) {
        for (const [order, profile] of sweep.profiles.entries()) {
          const profileId = profileKey(
            profile.thinking,
            profile.nanocodexToolMode,
            profile.codexToolMode,
          );
          const key = coordinateKey(task.name, profileId);
          const existing = catalog.get(key);
          if (existing && (existing.taskDigest !== task.contentDigest || existing.trials !== sweep.trials)) {
            throw new Error(`incompatible duplicate sweep coordinate for ${task.name}`);
          }
          catalog.set(key, {
            key,
            taskName: task.name,
            taskDigest: task.contentDigest,
            trials: sweep.trials,
            order,
            ...profile,
            profile: profileId,
          });
          if (!evidence.has(key)) evidence.set(key, { reports: [], active: [] });
        }
      }

      for (const entry of await fs.readdir(sweep.root, { withFileTypes: true })) {
        if (!entry.isDirectory() || entry.name.startsWith(".")) continue;
        const directory = path.join(sweep.root, entry.name);
        const reportPath = path.join(directory, REPORT);
        let report = this.reportCache.get(reportPath);
        if (!report) {
          try {
            report = parseReport(JSON.parse(await fs.readFile(reportPath, "utf8")), reportPath);
            this.reportCache.set(reportPath, report);
            this.detailIndex.set(report.detailId, report);
          } catch (error) {
            if (error?.code !== "ENOENT") throw error;
          }
        }
        if (report) {
          const key = coordinateKey(report.taskName, report.profile);
          evidence.get(key)?.reports.push(report);
          continue;
        }
        if (this.tombstones.has(directory)) continue;
        const coordinate = parseDirectoryCoordinate(entry.name, sweep);
        if (!coordinate) continue;
        const progress = await readProgress(path.join(directory, PROGRESS));
        if (!progress) continue;
        const ageMs = Math.max(0, nowMs - progress.latest.observedMs);
        const stale = ageMs > this.staleAfterMs;
        if (stale && !(await containsOverlay(directory))) {
          this.tombstones.add(directory);
          continue;
        }
        const key = coordinateKey(coordinate.taskName, coordinate.profile);
        evidence.get(key)?.active.push({ ...coordinate, directory, progress, stale, ageMs });
      }
    }

    const matrix = buildRows(catalog, evidence, nowMs);
    const host = await this.hostHealth();
    const evidenceAgeMs = matrix.latestEvidenceMs ? Math.max(0, nowMs - matrix.latestEvidenceMs) : null;
    const memoryAvailablePercent = host.totalBytes
      ? (host.availableBytes / host.totalBytes) * 100
      : null;
    const diskAvailablePercent = host.diskTotalBytes
      ? (host.diskAvailableBytes / host.diskTotalBytes) * 100
      : null;
    let status = "healthy";
    let statusMessage = "Evidence is flowing and host resources are within bounds.";
    if (matrix.counters.stale > 0 || (matrix.counters.running > 0 && evidenceAgeMs > this.staleAfterMs)) {
      status = "stalled";
      statusMessage = `${matrix.counters.stale || matrix.counters.running} active coordinate(s) are not producing fresh evidence.`;
    } else if (
      (memoryAvailablePercent !== null && memoryAvailablePercent < 10) ||
      (diskAvailablePercent !== null && diskAvailablePercent < 5) ||
      matrix.infrastructure15m >= Math.max(3, Math.ceil(matrix.reports15m * 0.05))
    ) {
      status = "degraded";
      statusMessage = "Work is progressing, but resource or infrastructure pressure needs attention.";
    }

    this.sequence += 1;
    const memoryUsedPercent = memoryAvailablePercent === null ? null : 100 - memoryAvailablePercent;
    const swapUsedPercent = host.swapTotalBytes
      ? (host.swapUsedBytes / host.swapTotalBytes) * 100
      : null;
    const diskUsedPercent = diskAvailablePercent === null ? null : 100 - diskAvailablePercent;
    return {
      schemaVersion: 1,
      sequence: this.sequence,
      observedAt: new Date(nowMs).toISOString(),
      sourceCount: this.sweeps.length,
      health: {
        status,
        statusMessage,
        evidenceAgeMs,
        completions5m: matrix.reports5m,
        infrastructure15m: matrix.infrastructure15m,
        host: {
          cpuPercent: host.cpuPercent,
          loadPercent: host.logicalCpus ? (host.load1 / host.logicalCpus) * 100 : null,
          memoryUsedPercent,
          swapUsedPercent,
          diskUsedPercent,
        },
      },
      summary: matrix.counters,
      rows: matrix.rows,
      recentFailures: matrix.recentFailures,
    };
  }
}
