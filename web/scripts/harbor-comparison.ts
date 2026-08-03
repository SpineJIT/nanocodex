import { createHash } from "node:crypto";

export type Digest = `sha256:${string}`;
export type Runner = "harness" | "codex";
type TrialStatus = "passed" | "failed" | "error";

export type ExperimentPolicy = {
  model: string;
  effort: string;
  systemInstructionsDigest: Digest;
  environmentDigest: Digest;
  verifierDigest: Digest;
  timeoutPolicyDigest: Digest;
  resourcePolicyDigest: Digest;
  toolAvailabilityDigest: Digest;
};

export type ExperimentAttempt = {
  attemptIndex: number;
  harnessTrialId: string;
  codexTrialId: string;
};

export type ExperimentTask = {
  name: string;
  digest: Digest;
  attempts: ExperimentAttempt[];
};

export type ExperimentPlan = {
  schemaVersion: 1;
  id: string;
  datasetDigest: Digest;
  attemptCount: number;
  tasks: ExperimentTask[];
  policy: ExperimentPolicy;
  arms: Record<
    Runner,
    {
      job: { key: string; id: string; lockDigest: Digest };
      candidateProvenanceDigest: Digest;
    }
  >;
};

export type ComparisonTrial = {
  id: string;
  trialName: string;
  taskName: string;
  datasetDigest: Digest;
  taskDigest: Digest;
  status: TrialStatus;
  reward: number | null;
  durationMs: number | null;
  agentDurationMs: number | null;
  model: string | null;
  effort: string | null;
  modelCalls: number;
  tokens: { input: number; cached: number; output: number };
};

export type ComparisonJob = {
  key: string;
  id: string;
  runner: Runner;
  lockDigest: Digest | null;
  experimentPlanDigest: Digest | null;
  candidateProvenanceDigest: Digest | null;
  policy: Partial<ExperimentPolicy>;
  name: string;
  branch: string;
  finishedAt: string;
  durationMs: number | null;
  trials: ComparisonTrial[];
};

const DIGEST = /^sha256:[0-9a-f]{64}$/;
const RUNNERS = ["harness", "codex"] as const;
const POLICY_DIGEST_FIELDS = [
  "systemInstructionsDigest",
  "environmentDigest",
  "verifierDigest",
  "timeoutPolicyDigest",
  "resourcePolicyDigest",
  "toolAvailabilityDigest",
] as const;

export function sha256(value: string): Digest {
  return `sha256:${createHash("sha256").update(value).digest("hex")}`;
}

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function assertDigest(value: unknown, field: string): asserts value is Digest {
  assert(typeof value === "string", `${field} must be a sha256 digest`);
  assert(DIGEST.test(value), `${field} must be a sha256 digest`);
}

export function validateExperimentPlan(
  value: unknown,
  filename?: string,
): ExperimentPlan {
  const plan = value as ExperimentPlan;
  assert(plan?.schemaVersion === 1, "unsupported experiment plan schema");
  assert(
    typeof plan.id === "string" && plan.id.length > 0,
    "plan.id is required",
  );
  assertDigest(plan.datasetDigest, "plan.datasetDigest");
  assert(
    Number.isInteger(plan.attemptCount) && plan.attemptCount > 0,
    "plan.attemptCount is invalid",
  );
  assert(
    Array.isArray(plan.tasks) && plan.tasks.length > 0,
    "plan.tasks is required",
  );
  const taskDigests = new Set<Digest>();
  const trialIds: Record<Runner, Set<string>> = {
    harness: new Set(),
    codex: new Set(),
  };
  for (const [index, task] of plan.tasks.entries()) {
    assert(
      typeof task.name === "string" && task.name.length > 0,
      `plan.tasks[${index}].name is required`,
    );
    assertDigest(task.digest, `plan.tasks[${index}].digest`);
    assert(
      !taskDigests.has(task.digest),
      `duplicate task digest ${task.digest}`,
    );
    taskDigests.add(task.digest);
    assert(
      Array.isArray(task.attempts) &&
        task.attempts.length === plan.attemptCount,
      `plan.tasks[${index}].attempts must contain ${plan.attemptCount} entries`,
    );
    const attemptIndexes = new Set<number>();
    for (const [attemptPosition, attempt] of task.attempts.entries()) {
      const field = `plan.tasks[${index}].attempts[${attemptPosition}]`;
      assert(
        Number.isInteger(attempt.attemptIndex) &&
          attempt.attemptIndex >= 0 &&
          attempt.attemptIndex < plan.attemptCount,
        `${field}.attemptIndex is invalid`,
      );
      assert(
        !attemptIndexes.has(attempt.attemptIndex),
        `${field}.attemptIndex is duplicated`,
      );
      attemptIndexes.add(attempt.attemptIndex);
      for (const runner of RUNNERS) {
        const id =
          runner === "harness" ? attempt.harnessTrialId : attempt.codexTrialId;
        assert(
          typeof id === "string" && id.length > 0,
          `${field}.${runner}TrialId is required`,
        );
        assert(
          !trialIds[runner].has(id),
          `${field}.${runner}TrialId is duplicated`,
        );
        trialIds[runner].add(id);
      }
    }
  }

  for (const field of POLICY_DIGEST_FIELDS) {
    assertDigest(plan.policy?.[field], `plan.policy.${field}`);
  }
  assert(
    typeof plan.policy?.model === "string" && plan.policy.model.length > 0,
    "plan.policy.model is required",
  );
  assert(
    typeof plan.policy?.effort === "string" && plan.policy.effort.length > 0,
    "plan.policy.effort is required",
  );

  for (const runner of RUNNERS) {
    const arm = plan.arms?.[runner];
    assert(arm, `plan.arms.${runner} is required`);
    assert(
      typeof arm.job?.key === "string" && arm.job.key.length > 0,
      `plan.arms.${runner}.job.key is required`,
    );
    assert(
      typeof arm.job?.id === "string" && arm.job.id.length > 0,
      `plan.arms.${runner}.job.id is required`,
    );
    assertDigest(arm.job.lockDigest, `plan.arms.${runner}.job.lockDigest`);
    assertDigest(
      arm.candidateProvenanceDigest,
      `plan.arms.${runner}.candidateProvenanceDigest`,
    );
  }

  if (filename) {
    const expected = `${sha256(`${JSON.stringify(plan)}\n`).slice("sha256:".length)}.json`;
    assert(
      filename === expected,
      `experiment plan filename must be its canonical digest: ${expected}`,
    );
  }
  return plan;
}

function validateJob(
  plan: ExperimentPlan,
  runner: Runner,
  jobs: ComparisonJob[],
  planDigest: Digest,
): ComparisonJob {
  const arm = plan.arms[runner];
  const job = jobs.find((candidate) => candidate.key === arm.job.key);
  assert(job, `${runner} job ${arm.job.key} was not retained`);
  assert(
    job.runner === runner,
    `${arm.job.key} has runner ${job.runner}, expected ${runner}`,
  );
  assert(job.id === arm.job.id, `${arm.job.key} job id changed`);
  assert(
    job.lockDigest === arm.job.lockDigest,
    `${arm.job.key} lock digest changed`,
  );
  assert(
    job.experimentPlanDigest === planDigest,
    `${arm.job.key} does not reference this experiment plan`,
  );
  assert(
    job.candidateProvenanceDigest === arm.candidateProvenanceDigest,
    `${arm.job.key} has the wrong candidate provenance`,
  );
  for (const field of POLICY_DIGEST_FIELDS) {
    assert(
      job.policy?.[field] === plan.policy[field],
      `${arm.job.key} has the wrong ${field}`,
    );
  }

  const expectedCount = plan.tasks.length * plan.attemptCount;
  assert(
    job.trials.length === expectedCount,
    `${arm.job.key} has ${job.trials.length} trials, expected ${expectedCount}`,
  );
  for (const trial of job.trials) {
    assert(
      trial.datasetDigest === plan.datasetDigest,
      `${trial.trialName} has the wrong dataset digest`,
    );
    assert(
      trial.model === plan.policy.model,
      `${trial.trialName} has the wrong model`,
    );
    assert(
      trial.effort === plan.policy.effort,
      `${trial.trialName} has the wrong effort`,
    );
  }
  return job;
}

function summarizeJob(job: ComparisonJob, trials: ComparisonTrial[]) {
  const rewards = trials.map((trial) => trial.reward ?? 0);
  return {
    key: job.key,
    name: job.name,
    branch: job.branch,
    finishedAt: job.finishedAt,
    durationMs: job.durationMs,
    agentDurationMs: trials.reduce(
      (total, trial) => total + (trial.agentDurationMs ?? 0),
      0,
    ),
    passed: trials.filter((trial) => trial.status === "passed").length,
    score:
      rewards.reduce((total, reward) => total + reward, 0) / rewards.length,
    model: trials[0]?.model ?? null,
    effort: trials[0]?.effort ?? null,
    modelCalls: trials.reduce((total, trial) => total + trial.modelCalls, 0),
    tokens: trials.reduce(
      (total, trial) => ({
        input: total.input + trial.tokens.input,
        cached: total.cached + trial.tokens.cached,
        output: total.output + trial.tokens.output,
      }),
      { input: 0, cached: 0, output: 0 },
    ),
  };
}

export function buildComparisonFromPlan(
  plan: ExperimentPlan,
  jobs: ComparisonJob[],
  planDigest: Digest,
) {
  validateExperimentPlan(plan);
  assertDigest(planDigest, "planDigest");
  const harnessJob = validateJob(plan, "harness", jobs, planDigest);
  const codexJob = validateJob(plan, "codex", jobs, planDigest);
  const trialsById = {
    harness: new Map(harnessJob.trials.map((trial) => [trial.id, trial])),
    codex: new Map(codexJob.trials.map((trial) => [trial.id, trial])),
  };
  assert(
    trialsById.harness.size === harnessJob.trials.length,
    "harness job has duplicate trial ids",
  );
  assert(
    trialsById.codex.size === codexJob.trials.length,
    "codex job has duplicate trial ids",
  );

  let harnessWins = 0;
  let codexWins = 0;
  let ties = 0;
  const usedTrialIds: Record<Runner, Set<string>> = {
    harness: new Set(),
    codex: new Set(),
  };
  const tasks = plan.tasks.flatMap((task) =>
    task.attempts.map((attempt) => {
      const harnessTrial = trialsById.harness.get(attempt.harnessTrialId);
      const codexTrial = trialsById.codex.get(attempt.codexTrialId);
      assert(
        harnessTrial,
        `harness job is missing trial ${attempt.harnessTrialId}`,
      );
      assert(codexTrial, `codex job is missing trial ${attempt.codexTrialId}`);
      for (const [runner, trial] of [
        ["harness", harnessTrial],
        ["codex", codexTrial],
      ] as const) {
        assert(
          trial.datasetDigest === plan.datasetDigest,
          `${trial.trialName} has the wrong dataset digest`,
        );
        assert(
          trial.taskDigest === task.digest,
          `${trial.trialName} has the wrong task digest`,
        );
        assert(
          trial.taskName === task.name,
          `${trial.trialName} has the wrong task name`,
        );
        usedTrialIds[runner].add(trial.id);
      }

      const harnessReward = harnessTrial.reward ?? 0;
      const codexReward = codexTrial.reward ?? 0;
      const outcome =
        harnessReward > codexReward
          ? "harness"
          : codexReward > harnessReward
            ? "codex"
            : "tie";
      if (outcome === "harness") harnessWins += 1;
      else if (outcome === "codex") codexWins += 1;
      else ties += 1;
      return {
        taskName: task.name,
        datasetDigest: plan.datasetDigest,
        taskDigest: task.digest,
        attemptIndex: attempt.attemptIndex,
        outcome,
        harness: {
          id: harnessTrial.id,
          status: harnessTrial.status,
          reward: harnessTrial.reward,
          durationMs: harnessTrial.durationMs,
        },
        codex: {
          id: codexTrial.id,
          status: codexTrial.status,
          reward: codexTrial.reward,
          durationMs: codexTrial.durationMs,
        },
      };
    }),
  );
  for (const runner of RUNNERS) {
    assert(
      usedTrialIds[runner].size === trialsById[runner].size,
      `${runner} job contains trials not assigned by the experiment plan`,
    );
  }
  tasks.sort(
    (left, right) =>
      left.taskName.localeCompare(right.taskName) ||
      left.attemptIndex - right.attemptIndex,
  );

  const harness = summarizeJob(harnessJob, harnessJob.trials);
  const codex = summarizeJob(codexJob, codexJob.trials);
  return {
    planId: plan.id,
    planDigest,
    datasetDigest: plan.datasetDigest,
    attemptCount: plan.attemptCount,
    taskCount: plan.tasks.length,
    pairCount: tasks.length,
    policy: plan.policy,
    candidateProvenance: {
      harness: plan.arms.harness.candidateProvenanceDigest,
      codex: plan.arms.codex.candidateProvenanceDigest,
    },
    harness,
    codex,
    delta: harness.score - codex.score,
    headToHead: { harness: harnessWins, codex: codexWins, ties },
    tasks,
  };
}

type ExperimentPlanRecord = { plan: ExperimentPlan; digest: Digest };
type Comparison = ReturnType<typeof buildComparisonFromPlan>;

export function selectComparison(
  plans: ExperimentPlanRecord[],
  jobs: ComparisonJob[],
): Comparison | null {
  const comparisons: Comparison[] = [];
  for (const { plan, digest } of plans) {
    try {
      comparisons.push(buildComparisonFromPlan(plan, jobs, digest));
    } catch (error: unknown) {
      console.warn(
        `Skipping Harbor comparison plan ${plan.id}: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }
  comparisons.sort((left, right) => {
    const leftFinishedAt = Math.max(
      new Date(left.harness.finishedAt).getTime(),
      new Date(left.codex.finishedAt).getTime(),
    );
    const rightFinishedAt = Math.max(
      new Date(right.harness.finishedAt).getTime(),
      new Date(right.codex.finishedAt).getTime(),
    );
    return rightFinishedAt - leftFinishedAt;
  });
  return comparisons[0] ?? null;
}
