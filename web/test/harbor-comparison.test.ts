import assert from "node:assert/strict";
import test from "node:test";
import {
  buildComparisonFromPlan,
  type ComparisonJob,
  type ComparisonTrial,
  type Digest,
  type ExperimentPlan,
  type Runner,
  validateExperimentPlan,
} from "../scripts/harbor-comparison.ts";

const digest = (character: string): Digest => `sha256:${character.repeat(64)}`;

function trial(
  runner: Runner,
  taskName: string,
  taskDigest: Digest,
  attemptIndex: number,
  reward: number,
): ComparisonTrial {
  return {
    id: `${runner}-${taskName}-${attemptIndex}`,
    trialName: `${taskName}-${runner}-${attemptIndex}`,
    taskName,
    datasetDigest: digest("a"),
    taskDigest,
    status: reward === 1 ? "passed" : "failed",
    reward,
    durationMs: 10,
    agentDurationMs: 8,
    model: "gpt-test",
    effort: "high",
    modelCalls: 1,
    tokens: { input: 2, cached: 1, output: 1 },
  };
}

function fixture() {
  const taskDigest = digest("b");
  const plan: ExperimentPlan = {
    schemaVersion: 1,
    id: "paired-test",
    datasetDigest: digest("a"),
    attemptCount: 2,
    tasks: [
      {
        name: "task-a",
        digest: taskDigest,
        attempts: [
          {
            attemptIndex: 0,
            harnessTrialId: "harness-task-a-0",
            codexTrialId: "codex-task-a-0",
          },
          {
            attemptIndex: 1,
            harnessTrialId: "harness-task-a-1",
            codexTrialId: "codex-task-a-1",
          },
        ],
      },
    ],
    policy: {
      model: "gpt-test",
      effort: "high",
      systemInstructionsDigest: digest("c"),
      environmentDigest: digest("d"),
      verifierDigest: digest("e"),
      timeoutPolicyDigest: digest("f"),
      resourcePolicyDigest: digest("0"),
      toolAvailabilityDigest: digest("1"),
    },
    arms: {
      harness: {
        job: { key: "harness-key", id: "harness-id", lockDigest: digest("2") },
        candidateProvenanceDigest: digest("3"),
      },
      codex: {
        job: { key: "codex-key", id: "codex-id", lockDigest: digest("4") },
        candidateProvenanceDigest: digest("5"),
      },
    },
  };
  const job = (runner: Runner, rewards: number[]): ComparisonJob => ({
    key: `${runner}-key`,
    id: `${runner}-id`,
    runner,
    lockDigest: plan.arms[runner].job.lockDigest,
    experimentPlanDigest: digest("6"),
    policy: plan.policy,
    candidateProvenanceDigest: plan.arms[runner].candidateProvenanceDigest,
    name: runner,
    branch: "main",
    finishedAt: "2026-01-01T00:00:00Z",
    durationMs: 20,
    trials: rewards.map((reward, index) =>
      trial(runner, "task-a", taskDigest, index, reward),
    ),
  });
  return { plan, jobs: [job("harness", [1, 0]), job("codex", [0, 1])] };
}

test("pairs every attempt by planned trial ids and reports the immutable tuple", () => {
  const { plan, jobs } = fixture();
  jobs[1].trials.reverse();
  const comparison = buildComparisonFromPlan(plan, jobs, digest("6"));
  assert.equal(comparison.taskCount, 1);
  assert.equal(comparison.pairCount, 2);
  assert.deepEqual(comparison.headToHead, { harness: 1, codex: 1, ties: 0 });
  assert.deepEqual(
    comparison.tasks.map((row) => row.attemptIndex),
    [0, 1],
  );
});

test("rejects a task revision mismatch instead of pairing by task name", () => {
  const { plan, jobs } = fixture();
  jobs[1].trials[0].taskDigest = digest("7");
  assert.throws(
    () => buildComparisonFromPlan(plan, jobs, digest("6")),
    /wrong task digest/,
  );
});

test("rejects missing, duplicate, or out-of-range attempt assignments", () => {
  for (const attempts of [
    [],
    [
      { attemptIndex: 0, harnessTrialId: "harness-a", codexTrialId: "codex-a" },
      { attemptIndex: 0, harnessTrialId: "harness-b", codexTrialId: "codex-b" },
    ],
    [
      { attemptIndex: 0, harnessTrialId: "harness-a", codexTrialId: "codex-a" },
      { attemptIndex: 2, harnessTrialId: "harness-b", codexTrialId: "codex-b" },
    ],
  ]) {
    const { plan } = fixture();
    plan.tasks[0].attempts = attempts;
    assert.throws(() => validateExperimentPlan(plan));
  }
});

test("rejects changed job identity, lock, policy, model, effort, or attempt count", () => {
  const mutations: Array<(jobs: ComparisonJob[]) => void> = [
    (jobs) => {
      jobs[0].id = "replacement";
    },
    (jobs) => {
      jobs[0].lockDigest = digest("8");
    },
    (jobs) => {
      jobs[0].policy = { ...jobs[0].policy, verifierDigest: digest("8") };
    },
    (jobs) => {
      jobs[0].candidateProvenanceDigest = digest("8");
    },
    (jobs) => {
      jobs[0].trials[0].model = "other";
    },
    (jobs) => {
      jobs[0].trials[0].effort = "low";
    },
    (jobs) => {
      jobs[0].trials.pop();
    },
  ];
  for (const mutate of mutations) {
    const { plan, jobs } = fixture();
    mutate(jobs);
    assert.throws(() => buildComparisonFromPlan(plan, jobs, digest("6")));
  }
});
