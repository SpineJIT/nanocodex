import { execFile } from "node:child_process";
import { mkdir, readFile, readdir, stat, writeFile } from "node:fs/promises";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { selectComparison, sha256, validateExperimentPlan } from "./harbor-comparison.ts";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryPath = resolve(
  process.env.NANOCODEX_REPO ?? resolve(projectRoot, ".."),
);
const outputDirectory = resolve(projectRoot, "public", "data", "harbor");
const detailDirectory = resolve(outputDirectory, "trials");
const indexPath = resolve(outputDirectory, "index.json");
const summaryPath = resolve(outputDirectory, "summary.json");
const homepageSummaryPath = resolve(projectRoot, "src", "data", "harbor-summary.json");
const experimentPlanDirectory = resolve(
  process.env.NANOCODEX_HARBOR_EXPERIMENTS ?? resolve(repositoryPath, "evals", "harbor-comparisons"),
);
const detailSchemaVersion = 3;
const execFileAsync = promisify(execFile);

async function readJson(path, fallback = null) {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch {
    return fallback;
  }
}

async function readJsonLines(path) {
  try {
    return (await readFile(path, "utf8"))
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line));
  } catch {
    return [];
  }
}

function millisecondsBetween(start, finish) {
  if (!start || !finish) return null;
  const duration = new Date(finish).getTime() - new Date(start).getTime();
  return Number.isFinite(duration) ? Math.max(0, duration) : null;
}

function safeSegment(value) {
  return value.replace(/[^a-zA-Z0-9._-]/g, "_");
}

function redact(value) {
  return value
    .replace(/(api[_-]?key|token|password|secret)\s*[=:]\s*[^\s]+/gi, "$1=[redacted]")
    .replace(/bearer\s+[^\s]+/gi, "Bearer [redacted]")
    .replace(/-----BEGIN [^-]+ PRIVATE KEY-----[\s\S]*/gi, "[private key redacted]");
}

function commandSummary(tool, argumentsValue) {
  if (!argumentsValue || typeof argumentsValue !== "object") return tool;
  const command = typeof argumentsValue.cmd === "string" ? argumentsValue.cmd : null;
  if (command) {
    const firstStatement = command
      .split("\n")
      .map((line) => line.trim())
      .find((line) => line && !/^(set\s+-|pwd$|cd\s+)/.test(line));
    return redact(firstStatement ?? tool).slice(0, 220);
  }
  const path = typeof argumentsValue.path === "string" ? argumentsValue.path : null;
  if (path) return `${tool} ${path}`.slice(0, 220);
  return tool;
}

function parseToolExitCode(event) {
  const encoded = event?.payload?.result?.output;
  if (typeof encoded !== "string") return null;
  try {
    const parsed = JSON.parse(encoded);
    return typeof parsed.exit_code === "number" ? parsed.exit_code : null;
  } catch {
    return null;
  }
}

function jobMean(result) {
  const evals = Object.values(result?.stats?.evals ?? {});
  const firstMetric = evals[0]?.metrics?.[0]?.mean;
  return typeof firstMetric === "number" ? firstMetric : null;
}

function runnerFromResult(result, fallback) {
  const evalName = Object.keys(result?.stats?.evals ?? {})[0] ?? "";
  if (evalName.startsWith("codex__") || result?.agent_info?.name === "codex") return "codex";
  if (
    evalName.startsWith("nanocodex__") ||
    evalName.startsWith("harness__") ||
    result?.agent_info?.name === "nanocodex" ||
    result?.agent_info?.name === "harness"
  ) {
    return "harness";
  }
  return fallback;
}

async function discoverWorktrees() {
  try {
    const { stdout } = await execFileAsync("git", [
      "-C",
      repositoryPath,
      "worktree",
      "list",
      "--porcelain",
    ]);
    const worktrees = [];
    let current = null;
    for (const line of stdout.split("\n")) {
      if (line.startsWith("worktree ")) {
        if (current) worktrees.push(current);
        current = { path: line.slice("worktree ".length), branch: "detached" };
      } else if (current && line.startsWith("branch refs/heads/")) {
        current.branch = line.slice("branch refs/heads/".length);
      }
    }
    if (current) worktrees.push(current);
    return worktrees.length ? worktrees : [{ path: repositoryPath, branch: "main" }];
  } catch {
    return [{ path: repositoryPath, branch: "main" }];
  }
}

async function discoverJobStores() {
  const worktrees = await discoverWorktrees();
  const stores = [];
  for (const worktree of worktrees) {
    const branchKey = safeSegment(worktree.branch);
    for (const artifactRoot of [".nanocodex", ".harness"]) {
      const rootKey = artifactRoot.slice(1);
      stores.push({
        path: resolve(worktree.path, artifactRoot, "harbor", "jobs"),
        worktreePath: worktree.path,
        branch: worktree.branch,
        key: `${branchKey}-${rootKey}`,
        source: `${worktree.branch}/${artifactRoot}/harbor/jobs`,
        fallbackRunner: "harness",
      });
      stores.push({
        path: resolve(worktree.path, artifactRoot, "harbor", "codex-jobs"),
        worktreePath: worktree.path,
        branch: worktree.branch,
        key: `${branchKey}-${rootKey}-codex`,
        source: `${worktree.branch}/${artifactRoot}/harbor/codex-jobs`,
        fallbackRunner: "codex",
      });
    }
  }
  return stores;
}

async function buildTrial(jobName, trialName, trialPath, context) {
  const resultPath = resolve(trialPath, "result.json");
  const ctrfPath = resolve(trialPath, "verifier", "ctrf.json");
  const inputPath = resolve(trialPath, "agent", "input.jsonl");
  const eventsPath = resolve(trialPath, "agent", "events.jsonl");
  const codexTrajectoryPath = resolve(trialPath, "agent", "trajectory.json");
  const result = await readJson(resultPath);
  if (!result) return null;

  const ctrf = await readJson(ctrfPath, {});
  const codexTrajectory = await readJson(codexTrajectoryPath, {});
  const codexAgentSteps = (codexTrajectory?.steps ?? []).filter(
    (step) => step.source === "agent",
  );
  const codexToolCalls = codexAgentSteps.flatMap((step) => step.tool_calls ?? []);
  const verifierSummary = ctrf?.results?.summary ?? null;
  const reward = result?.verifier_result?.rewards?.reward;
  const status = result.exception_info
    ? "error"
    : typeof reward === "number" && reward >= 1
      ? "passed"
      : "failed";
  const requestId = result.id ?? `${jobName}-${trialName}`;
  const safeJobName = safeSegment(`${context.storeKey}-${context.runner}-${jobName}`);
  const safeTrialName = safeSegment(trialName);
  const detailUrl = `/data/harbor/trials/${safeJobName}/${safeTrialName}.json`;
  const detailPath = resolve(detailDirectory, safeJobName, `${safeTrialName}.json`);
  const metadata = result?.agent_result?.metadata ?? {};
  const summary = {
    id: requestId,
    taskName: result.task_name ?? trialName.split("__")[0],
    trialName,
    source: result.source ?? null,
    taskRef: result?.task_id?.ref ?? null,
    taskDigest: result.task_checksum ?? null,
    status,
    reward: typeof reward === "number" ? reward : null,
    startedAt: result.started_at ?? null,
    finishedAt: result.finished_at ?? null,
    durationMs: millisecondsBetween(result.started_at, result.finished_at),
    agentDurationMs: millisecondsBetween(
      result?.agent_execution?.started_at,
      result?.agent_execution?.finished_at,
    ),
    model: result?.agent_info?.model_info?.name ?? metadata.model ?? null,
    effort:
      metadata.effort ??
      result?.config?.agent?.kwargs?.effort ??
      result?.config?.agent?.kwargs?.reasoning_effort ??
      null,
    runner: context.runner,
    branch: context.branch,
    modelCalls: metadata.model_calls ?? codexAgentSteps.length,
    toolCalls: metadata.tool_calls ?? codexToolCalls.length,
    tokens: {
      input: result?.agent_result?.n_input_tokens ?? 0,
      cached: result?.agent_result?.n_cache_tokens ?? 0,
      output: result?.agent_result?.n_output_tokens ?? 0,
      reasoning:
        metadata.reasoning_output_tokens ??
        codexTrajectory?.final_metrics?.extra?.reasoning_output_tokens ??
        0,
    },
    verifier: verifierSummary
      ? {
          tests: verifierSummary.tests ?? 0,
          passed: verifierSummary.passed ?? 0,
          failed: verifierSummary.failed ?? 0,
          skipped: verifierSummary.skipped ?? 0,
        }
      : null,
    detailUrl,
  };

  const [existingDetail, detailStat, ...sourceStats] = await Promise.all([
    readJson(detailPath),
    stat(detailPath).catch(() => null),
    ...[resultPath, ctrfPath, inputPath, eventsPath, codexTrajectoryPath].map((path) =>
      stat(path).catch(() => null),
    ),
  ]);
  const newestSource = Math.max(...sourceStats.map((sourceStat) => sourceStat?.mtimeMs ?? 0));
  if (
    existingDetail?.schemaVersion === detailSchemaVersion &&
    detailStat &&
    detailStat.mtimeMs >= newestSource
  ) {
    return summary;
  }

  const [inputEvents, events] = await Promise.all([
    readJsonLines(inputPath),
    readJsonLines(eventsPath),
  ]);
  const prompt = inputEvents.find((input) => typeof input.instruction === "string");
  const finalMessage = events.findLast((event) => event.type === "assistant.message");
  const codexInstruction = (codexTrajectory?.steps ?? []).findLast(
    (step) =>
      step.source === "user" &&
      typeof step.message === "string" &&
      !step.message.startsWith("<environment_context>"),
  );
  const codexFinalMessage = codexAgentSteps.findLast(
    (step) => typeof step.message === "string" && step.message.length > 0,
  );
  const resultByCallId = new Map(
    events
      .filter((event) => event.type === "tool.result")
      .map((event) => [event?.payload?.call_id, event]),
  );
  const harnessTrajectory = events
    .filter((event) => event.type === "tool.call")
    .map((event, index) => {
      const callResult = resultByCallId.get(event?.payload?.call_id);
      const argumentsValue = event?.payload?.arguments;
      return {
        index: index + 1,
        modelCall: event?.payload?.model_call_index ?? null,
        tool: event?.payload?.tool ?? "tool",
        summary: commandSummary(event?.payload?.tool ?? "tool", argumentsValue),
        workdir:
          argumentsValue && typeof argumentsValue === "object" &&
          typeof argumentsValue.workdir === "string"
            ? argumentsValue.workdir
            : null,
        status: callResult?.payload?.status ?? "unknown",
        durationMs:
          typeof callResult?.payload?.duration_ns === "number"
            ? callResult.payload.duration_ns / 1_000_000
            : null,
        exitCode: parseToolExitCode(callResult),
      };
    });
  const retainedTrajectory = harnessTrajectory.length
    ? harnessTrajectory
    : codexAgentSteps.flatMap((step, modelCall) =>
        (step.tool_calls ?? []).map((toolCall) => ({
          index: 0,
          modelCall: modelCall + 1,
          tool: toolCall.function_name ?? "tool",
          summary: toolCall.function_name ?? "tool",
          workdir: null,
          status: step.observation ? "completed" : "unknown",
          durationMs: null,
          exitCode: null,
        })),
      ).map((step, index) => ({ ...step, index: index + 1 }));

  const phases = [
    ["Environment", result.environment_setup],
    ["Agent setup", result.agent_setup],
    ["Agent", result.agent_execution],
    ["Verifier", result.verifier],
  ].map(([name, phase]) => ({
    name,
    durationMs: millisecondsBetween(phase?.started_at, phase?.finished_at),
  }));

  const detail = {
    schemaVersion: detailSchemaVersion,
    ...summary,
    instruction: prompt?.instruction ?? codexInstruction?.message ?? null,
    finalMessage: finalMessage?.payload?.text ?? codexFinalMessage?.message ?? null,
    exception: result.exception_info
      ? {
          type: result.exception_info.exception_type ?? "Error",
          message: redact(result.exception_info.exception_message ?? "Trial failed"),
        }
      : null,
    phases,
    transport: metadata.transport ?? null,
    orchestration: metadata.orchestration ?? null,
    reconnects: metadata.websocket_reconnects ?? 0,
    compactions: metadata.compactions ?? 0,
    trajectory: retainedTrajectory,
    verifierTests: (ctrf?.results?.tests ?? []).map((test) => ({
      name: test.name,
      status: test.status,
      durationMs: typeof test.duration === "number" ? test.duration * 1000 : null,
      filePath: test.file_path ?? null,
    })),
  };

  await mkdir(dirname(detailPath), { recursive: true });
  await writeFile(detailPath, `${JSON.stringify(detail)}\n`, "utf8");
  return summary;
}

function resolvedDatasetDigest(config, trials) {
  const refs = (config?.datasets ?? []).map((dataset) => dataset.ref).filter(Boolean);
  if (refs.length === 1 && /^sha256:[0-9a-f]{64}$/.test(refs[0])) return refs[0];
  const resolvedTasks = [...new Set(trials.map((trial) => JSON.stringify([trial.source, trial.taskDigest])))]
    .sort()
    .map((task) => JSON.parse(task));
  return sha256(`${JSON.stringify(resolvedTasks)}\n`);
}

function assignDatasetDigest(trials, datasetDigest) {
  for (const trial of trials) trial.datasetDigest = datasetDigest;
}

function canonicalValue(value) {
  if (Array.isArray(value)) return value.map(canonicalValue);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value)
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([key, item]) => [key, canonicalValue(item)]),
    );
  }
  return value;
}

function fingerprint(value) {
  return sha256(`${JSON.stringify(canonicalValue(value))}\n`);
}

function uniformFingerprint(values) {
  const fingerprints = [...new Set(values.map(fingerprint))];
  return fingerprints.length === 1 ? fingerprints[0] : null;
}

async function systemInstructionsEvidence(trials, worktreePath) {
  const evidence = [];
  for (const trial of trials) {
    const systemPromptPath = trial.agent?.kwargs?.system_prompt_path;
    if (!systemPromptPath) return null;
    const agentsMdPath = trial.agent?.kwargs?.agents_md_path;
    const systemPrompt = await readFile(resolve(worktreePath, systemPromptPath), "utf8").catch(() => null);
    const agentsMd = agentsMdPath
      ? await readFile(resolve(worktreePath, agentsMdPath), "utf8").catch(() => null)
      : null;
    if (systemPrompt === null || (agentsMdPath && agentsMd === null)) return null;
    evidence.push({
      systemPromptDigest: sha256(systemPrompt),
      agentsMdDigest: agentsMd === null ? null : sha256(agentsMd),
      extraInstructions: trial.extra_instructions ?? [],
    });
  }
  return uniformFingerprint(evidence);
}

async function policyEvidence(lock, worktreePath) {
  const trials = lock?.trials ?? [];
  const timeoutPolicy = (trial) => ({
    timeoutMultiplier: trial.timeout_multiplier ?? 1,
    agentTimeoutMultiplier: trial.agent_timeout_multiplier ?? null,
    verifierTimeoutMultiplier: trial.verifier_timeout_multiplier ?? null,
    agentSetupTimeoutMultiplier: trial.agent_setup_timeout_multiplier ?? null,
    environmentBuildTimeoutMultiplier: trial.environment_build_timeout_multiplier ?? null,
  });
  const resourcePolicy = (trial) => ({
    cpuEnforcementPolicy: trial.environment?.cpu_enforcement_policy ?? "auto",
    memoryEnforcementPolicy: trial.environment?.memory_enforcement_policy ?? "auto",
    cpus: trial.environment?.override_cpus ?? null,
    memoryMb: trial.environment?.override_memory_mb ?? null,
    storageMb: trial.environment?.override_storage_mb ?? null,
    gpus: trial.environment?.override_gpus ?? null,
    tpu: trial.environment?.override_tpu ?? null,
  });
  const toolAvailability = (trial) => ({
    skills: trial.skills ?? [],
    mcpServers: trial.agent?.mcp_servers ?? [],
    webSearch: [true, "enabled"].includes(trial.agent?.kwargs?.web_search),
    subagents: trial.agent?.kwargs?.subagents ?? false,
  });
  return {
    systemInstructionsDigest: await systemInstructionsEvidence(trials, worktreePath),
    environmentDigest: uniformFingerprint(trials.map((trial) => trial.environment)),
    verifierDigest: uniformFingerprint(trials.map((trial) => trial.verifier)),
    timeoutPolicyDigest: uniformFingerprint(trials.map(timeoutPolicy)),
    resourcePolicyDigest: uniformFingerprint(trials.map(resourcePolicy)),
    toolAvailabilityDigest: uniformFingerprint(trials.map(toolAvailability)),
  };
}

function candidateProvenanceEvidence(lock, runner) {
  const candidates = (lock?.trials ?? []).map((trial) => {
    const agent = trial.agent ?? {};
    const kwargs = agent.kwargs ?? {};
    const artifact = runner === "codex"
      ? { version: kwargs.version ?? null }
      : { binaryUrl: kwargs.binary_url ?? null, binarySha256: kwargs.binary_sha256 ?? null };
    if (runner === "codex" ? !artifact.version : !artifact.binarySha256) return null;
    return {
      name: agent.name ?? null,
      importPath: agent.import_path ?? null,
      artifact,
    };
  });
  return candidates.includes(null) ? null : uniformFingerprint(candidates);
}

async function readExperimentPlans() {
  const entries = await readdir(experimentPlanDirectory, { withFileTypes: true }).catch(() => []);
  const plans = [];
  for (const entry of entries) {
    if (!entry.isFile() || !entry.name.endsWith(".json")) continue;
    const path = resolve(experimentPlanDirectory, entry.name);
    const contents = await readFile(path, "utf8");
    const digest = sha256(contents);
    const expectedFilename = `${digest.slice("sha256:".length)}.json`;
    if (basename(path) !== expectedFilename) {
      console.warn(`Skipping Harbor comparison plan ${path}: filename must be ${expectedFilename}`);
      continue;
    }
    try {
      plans.push({ plan: validateExperimentPlan(JSON.parse(contents), entry.name), digest });
    } catch (error) {
      console.warn(`Skipping Harbor comparison plan ${path}: ${error.message}`);
    }
  }
  return plans;
}

const jobs = [];

for (const store of await discoverJobStores()) {
  const jobEntries = await readdir(store.path, { withFileTypes: true }).catch(() => []);
  for (const jobEntry of jobEntries) {
    if (!jobEntry.isDirectory()) continue;
    const jobPath = resolve(store.path, jobEntry.name);
    const result = await readJson(resolve(jobPath, "result.json"));
    if (!result?.finished_at) continue;
    const [config, lockContents] = await Promise.all([
      readJson(resolve(jobPath, "config.json"), {}),
      readFile(resolve(jobPath, "lock.json"), "utf8").catch(() => null),
    ]);
    const lock = lockContents ? JSON.parse(lockContents) : null;
    const experimentPlan = await readJson(resolve(jobPath, "experiment-plan.json"));
    const runner = runnerFromResult(result, store.fallbackRunner);

    const trialEntries = await readdir(jobPath, { withFileTypes: true });
    const trials = [];
    for (const trialEntry of trialEntries) {
      if (!trialEntry.isDirectory()) continue;
      const trialPath = resolve(jobPath, trialEntry.name);
      const trial = await buildTrial(jobEntry.name, trialEntry.name, trialPath, {
        branch: store.branch,
        runner,
        storeKey: store.key,
      });
      if (trial) trials.push(trial);
    }

    trials.sort((left, right) => (left.taskName ?? "").localeCompare(right.taskName ?? ""));
    const datasetDigest = resolvedDatasetDigest(config, trials);
    assignDatasetDigest(trials, datasetDigest);
    jobs.push({
      key: `${store.key}:${runner}:${jobEntry.name}`,
      id: result.id ?? `${store.key}-${jobEntry.name}`,
      name: jobEntry.name,
      runner,
      branch: store.branch,
      source: store.source,
      startedAt: result.started_at,
      finishedAt: result.finished_at,
      durationMs: millisecondsBetween(result.started_at, result.finished_at),
      totalTrials: result.n_total_trials ?? trials.length,
      completedTrials: result?.stats?.n_completed_trials ?? trials.length,
      erroredTrials: result?.stats?.n_errored_trials ?? 0,
      retries: result?.stats?.n_retries ?? 0,
      lockDigest: lockContents ? sha256(lockContents) : null,
      experimentPlanDigest: experimentPlan?.digest ?? null,
      policy: await policyEvidence(lock, store.worktreePath),
      candidateProvenanceDigest: candidateProvenanceEvidence(lock, runner),
      mean: jobMean(result),
      tokens: {
        input: result?.stats?.n_input_tokens ?? 0,
        cached: result?.stats?.n_cache_tokens ?? 0,
        output: result?.stats?.n_output_tokens ?? 0,
      },
      trials,
    });
  }
}

jobs.sort((left, right) => new Date(right.finishedAt).getTime() - new Date(left.finishedAt).getTime());

const comparison = selectComparison(await readExperimentPlans(), jobs);
const index = {
  generatedAt: new Date().toISOString(),
  source: "linked worktree Harbor records",
  jobCount: jobs.length,
  trialCount: jobs.reduce((count, job) => count + job.trials.length, 0),
  comparison,
  jobs,
};
const summary = {
  generatedAt: index.generatedAt,
  jobCount: index.jobCount,
  trialCount: index.trialCount,
  comparison: index.comparison,
};

await mkdir(outputDirectory, { recursive: true });
await mkdir(dirname(homepageSummaryPath), { recursive: true });
await Promise.all([
  writeFile(indexPath, `${JSON.stringify(index)}\n`, "utf8"),
  writeFile(summaryPath, `${JSON.stringify(summary)}\n`, "utf8"),
  writeFile(homepageSummaryPath, `${JSON.stringify(summary)}\n`, "utf8"),
]);
console.log(
  `Synced ${index.jobCount} Harbor jobs and ${index.trialCount} trials${
    comparison ? `; matched ${comparison.pairCount} paired Nanocodex/Codex attempts` : ""
  }`,
);
