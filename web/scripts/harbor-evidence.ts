import {
  sha256,
  type Digest,
  type ExperimentPolicy,
  type Runner,
} from "./harbor-comparison.ts";

type Trial = {
  extra_instructions?: unknown;
  environment?: unknown;
  verifier?: unknown;
  skills?: unknown;
  timeout_multiplier?: unknown;
  agent_timeout_multiplier?: unknown;
  verifier_timeout_multiplier?: unknown;
  agent_setup_timeout_multiplier?: unknown;
  environment_build_timeout_multiplier?: unknown;
  agent?: {
    name?: unknown;
    import_path?: unknown;
    mcp_servers?: unknown;
    kwargs?: Record<string, unknown>;
  };
};

type RuntimeTrial = {
  runtimeInstructionEvidence?: {
    systemPromptDigest?: unknown;
    agentsMdDigest?: unknown;
  };
};

type Lock = { trials?: Trial[] };

const DIGEST = /^sha256:[0-9a-f]{64}$/;
const EXACT_SEMVER =
  /^\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

function isDigest(value: unknown): value is Digest {
  return typeof value === "string" && DIGEST.test(value);
}

function canonicalValue(value: unknown): unknown {
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

function fingerprint(value: unknown): Digest {
  return sha256(`${JSON.stringify(canonicalValue(value))}\n`);
}

function uniformFingerprint(values: unknown[]): Digest | null {
  const fingerprints = [...new Set(values.map(fingerprint))];
  return fingerprints.length === 1 ? fingerprints[0] : null;
}

function systemInstructionsEvidence(
  trials: Trial[],
  runtimeTrials: RuntimeTrial[],
): Digest | null {
  if (trials.length === 0 || trials.length !== runtimeTrials.length) return null;
  if (!trials.every((trial) => trial.agent?.kwargs?.system_prompt_path)) {
    return null;
  }
  const agentsMdConfigured = trials.map((trial) =>
    Boolean(trial.agent?.kwargs?.agents_md_path),
  );
  if (new Set(agentsMdConfigured).size !== 1) return null;
  const requireAgentsMd = agentsMdConfigured[0];
  const contentEvidence = [];
  for (const runtimeTrial of runtimeTrials) {
    const runtime = runtimeTrial.runtimeInstructionEvidence;
    const systemPromptDigest = runtime?.systemPromptDigest;
    const agentsMdDigest = runtime?.agentsMdDigest;
    if (!isDigest(systemPromptDigest)) return null;
    if (requireAgentsMd ? !isDigest(agentsMdDigest) : agentsMdDigest !== null) {
      return null;
    }
    contentEvidence.push({
      systemPromptDigest,
      agentsMdDigest: agentsMdDigest ?? null,
    });
  }
  const contentDigest = uniformFingerprint(contentEvidence);
  const extraInstructionsDigest = uniformFingerprint(
    trials.map((trial) => trial.extra_instructions ?? []),
  );
  return contentDigest && extraInstructionsDigest
    ? fingerprint({ contentDigest, extraInstructionsDigest })
    : null;
}

function enabled(value: unknown, defaultValue = false): boolean {
  if (value === undefined) return defaultValue;
  return value === true || value === "enabled" || value === "true";
}

export function policyEvidence(
  lock: Lock | null,
  runner: Runner,
  runtimeTrials: RuntimeTrial[],
): Partial<ExperimentPolicy> {
  const trials = lock?.trials ?? [];
  const timeoutPolicy = (trial: Trial) => ({
    timeoutMultiplier: trial.timeout_multiplier ?? 1,
    agentTimeoutMultiplier: trial.agent_timeout_multiplier ?? null,
    verifierTimeoutMultiplier: trial.verifier_timeout_multiplier ?? null,
    agentSetupTimeoutMultiplier: trial.agent_setup_timeout_multiplier ?? null,
    environmentBuildTimeoutMultiplier:
      trial.environment_build_timeout_multiplier ?? null,
  });
  const resourcePolicy = (trial: Trial) => ({
    cpuEnforcementPolicy:
      (trial.environment as Record<string, unknown> | undefined)
        ?.cpu_enforcement_policy ?? "auto",
    memoryEnforcementPolicy:
      (trial.environment as Record<string, unknown> | undefined)
        ?.memory_enforcement_policy ?? "auto",
    cpus:
      (trial.environment as Record<string, unknown> | undefined)?.override_cpus ??
      null,
    memoryMb:
      (trial.environment as Record<string, unknown> | undefined)
        ?.override_memory_mb ?? null,
    storageMb:
      (trial.environment as Record<string, unknown> | undefined)
        ?.override_storage_mb ?? null,
    gpus:
      (trial.environment as Record<string, unknown> | undefined)?.override_gpus ??
      null,
    tpu:
      (trial.environment as Record<string, unknown> | undefined)?.override_tpu ??
      null,
  });
  const toolAvailability = (trial: Trial) => ({
    skills: trial.skills ?? [],
    mcpServers: trial.agent?.mcp_servers ?? [],
    webSearch: enabled(
      trial.agent?.kwargs?.web_search,
      runner === "harness",
    ),
    subagents: enabled(trial.agent?.kwargs?.subagents),
    installNode: enabled(trial.agent?.kwargs?.install_node),
  });
  return {
    systemInstructionsDigest:
      systemInstructionsEvidence(trials, runtimeTrials) ?? undefined,
    environmentDigest: uniformFingerprint(trials.map((trial) => trial.environment)) ?? undefined,
    verifierDigest: uniformFingerprint(trials.map((trial) => trial.verifier)) ?? undefined,
    timeoutPolicyDigest: uniformFingerprint(trials.map(timeoutPolicy)) ?? undefined,
    resourcePolicyDigest: uniformFingerprint(trials.map(resourcePolicy)) ?? undefined,
    toolAvailabilityDigest: uniformFingerprint(trials.map(toolAvailability)) ?? undefined,
  };
}

export function candidateProvenanceEvidence(
  lock: Lock | null,
  runner: Runner,
): Digest | null {
  const candidates = (lock?.trials ?? []).map((trial) => {
    const agent = trial.agent ?? {};
    const kwargs = agent.kwargs ?? {};
    const version = kwargs.version;
    const artifact =
      runner === "codex"
        ? typeof version === "string" && EXACT_SEMVER.test(version)
          ? { version }
          : null
        : {
            binaryUrl: kwargs.binary_url ?? null,
            binarySha256: kwargs.binary_sha256 ?? null,
          };
    if (
      artifact === null ||
      (runner === "harness" && !artifact.binarySha256)
    ) {
      return null;
    }
    return {
      name: agent.name ?? null,
      importPath: agent.import_path ?? null,
      artifact,
    };
  });
  return candidates.includes(null) ? null : uniformFingerprint(candidates);
}

export function parseRetainedLock(
  contents: string | null,
  path: string,
  warn: (message: string) => void = console.warn,
): Lock | null {
  if (contents === null) return null;
  try {
    const value = JSON.parse(contents);
    return value && typeof value === "object" ? value : null;
  } catch (error: unknown) {
    warn(
      `Skipping malformed Harbor lock ${path}: ${error instanceof Error ? error.message : String(error)}`,
    );
    return null;
  }
}
