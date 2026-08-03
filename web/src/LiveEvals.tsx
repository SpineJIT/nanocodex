import {
  Activity,
  AlertTriangle,
  CheckCircle2,
  CircleDashed,
  Cpu,
  Gauge,
  HardDrive,
  MemoryStick,
  Radio,
  RotateCw,
  Search,
  Server,
  X,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import "./evals.css";

type HealthStatus = "healthy" | "degraded" | "stalled";
type CellState = "queued" | "running" | "retrying" | "completed" | "stale";
type ArmState = "queued" | "running" | "passed" | "failed" | "error" | "unknown";
type MatrixFilter = "all" | "active" | "issues" | "complete";

type LivePhase = {
  phase: string;
  kind: string;
  summary: string | null;
  elapsedMs: number;
  observedAt: string | null;
};

type LiveCell = {
  trial: number;
  state: CellState;
  attempts: number;
  classification: string | null;
  nanocodex: ArmState;
  codex: ArmState;
  nanocodexPhase: LivePhase | null;
  codexPhase: LivePhase | null;
  updatedAt: string | null;
  durationMs: number | null;
  message: string | null;
  detailId: string | null;
};

type EvidenceText = { text: string; truncated: boolean } | null;

type CaseArm = {
  status: string;
  outcome: string;
  model: string | null;
  durationMs: number | null;
  toolCalls: number | null;
  usage: {
    inputTokens: number;
    cachedInputTokens: number;
    outputTokens: number;
    reasoningOutputTokens: number;
    totalTokens: number;
  };
  memory: {
    guestUsedPercent: number | null;
    oomDetected: boolean;
  };
  exception: {
    kind: string | null;
    outcome: string | null;
    message: string | null;
    occurredAt: string | null;
  } | null;
  verifier: {
    exitCode: number | null;
    rewards: Record<string, unknown>;
    stdout: EvidenceText;
    stderr: EvidenceText;
  };
  finalMessage: string | null;
};

type CaseDetail = {
  schemaVersion: number;
  detailId: string;
  task: {
    name: string;
    contentDigest: string;
    instruction: EvidenceText;
  };
  requestedTrial: number;
  actualTrial: number;
  thinking: string;
  profileLabel: string;
  classification: string;
  finishedAt: string | null;
  durationMs: number | null;
  history: Array<{
    trial: number;
    replacementFor: number | null;
    classification: string;
    nanocodexOutcome: string;
    codexOutcome: string;
    finishedAt: string | null;
    durationMs: number | null;
  }>;
  nanocodex: CaseArm;
  codex: CaseArm;
};

type LiveRow = {
  taskName: string;
  taskLabel: string;
  taskDigest: string;
  profile: string;
  profileLabel: string;
  thinking: string;
  nanocodexToolMode: string;
  codexToolMode: string;
  cells: LiveCell[];
};

export type LiveEvalSnapshot = {
  schemaVersion: number;
  sequence: number;
  observedAt: string;
  sourceCount: number;
  health: {
    status: HealthStatus;
    statusMessage: string;
    evidenceAgeMs: number | null;
    completions5m: number;
    infrastructure15m: number;
    host: {
      cpuPercent: number | null;
      loadPercent: number | null;
      memoryUsedPercent: number | null;
      swapUsedPercent: number | null;
      diskUsedPercent: number | null;
    };
  };
  summary: {
    total: number;
    completed: number;
    running: number;
    retrying: number;
    queued: number;
    stale: number;
    bothPassed: number;
    nanocodexOnly: number;
    codexOnly: number;
    neitherPassed: number;
  };
  rows: LiveRow[];
  recentFailures: Array<{
    taskName: string;
    trial: number;
    profileLabel: string;
    classification: string;
    nanocodex: string;
    codex: string;
    message: string | null;
    finishedAt: string | null;
  }>;
};

type LiveEvalConnection = {
  availability: "checking" | "available" | "unavailable";
  connected: boolean;
  snapshot: LiveEvalSnapshot | null;
};

export function useLiveEvalSnapshot(): LiveEvalConnection {
  const [state, setState] = useState<LiveEvalConnection>({
    availability: "checking",
    connected: false,
    snapshot: null,
  });

  useEffect(() => {
    const controller = new AbortController();
    let events: EventSource | null = null;
    let retry: number | null = null;
    const connect = () => {
      fetch("/api/evals", { cache: "no-store", signal: controller.signal })
        .then((response) => {
          if (!response.ok) throw new Error(`live eval endpoint returned ${response.status}`);
          return response.json() as Promise<LiveEvalSnapshot>;
        })
        .then((snapshot) => {
          setState({ availability: "available", connected: true, snapshot });
          events = new EventSource("/api/evals/events");
          events.addEventListener("snapshot", (event) => {
            const next = JSON.parse((event as MessageEvent<string>).data) as LiveEvalSnapshot;
            setState({ availability: "available", connected: true, snapshot: next });
          });
          events.onopen = () =>
            setState((current) => ({ ...current, availability: "available", connected: true }));
          events.onerror = () =>
            setState((current) => ({ ...current, availability: "available", connected: false }));
        })
        .catch((error) => {
          if (error instanceof DOMException && error.name === "AbortError") return;
          setState({ availability: "unavailable", connected: false, snapshot: null });
          retry = window.setTimeout(connect, 2_000);
        });
    };
    connect();
    return () => {
      controller.abort();
      if (retry !== null) window.clearTimeout(retry);
      events?.close();
    };
  }, []);

  return state;
}

function formatDuration(milliseconds: number | null) {
  if (milliseconds === null) return "—";
  if (milliseconds < 1_000) return `${Math.round(milliseconds)}ms`;
  const seconds = milliseconds / 1_000;
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${Math.round(seconds % 60)}s`;
}

function formatAge(milliseconds: number | null) {
  if (milliseconds === null) return "no evidence";
  if (milliseconds < 1_000) return "now";
  if (milliseconds < 60_000) return `${Math.floor(milliseconds / 1_000)}s ago`;
  return `${Math.floor(milliseconds / 60_000)}m ago`;
}

function formatPercent(value: number | null) {
  return value === null ? "—" : `${Math.round(value)}%`;
}

function availablePercent(used: number | null) {
  return used === null ? "unknown" : `${Math.max(0, Math.round(100 - used))}% headroom`;
}

function cellTitle(row: LiveRow, cell: LiveCell) {
  const lines = [
    `${row.taskLabel} · ${row.profileLabel}${cell.trial}`,
    `${cell.state}${cell.classification ? ` · ${cell.classification.replaceAll("_", " ")}` : ""}`,
    `Nanocodex: ${cell.nanocodexPhase?.phase ?? cell.nanocodex}`,
    `Codex: ${cell.codexPhase?.phase ?? cell.codex}`,
    `Attempts: ${cell.attempts || 0} · elapsed ${formatDuration(cell.durationMs)}`,
  ];
  if (cell.message) lines.push(cell.message);
  return lines.join("\n");
}

function rowMatchesFilter(row: LiveRow, filter: MatrixFilter) {
  if (filter === "all") return true;
  if (filter === "active") return row.cells.some((cell) => cell.state === "running");
  if (filter === "issues") {
    return row.cells.some(
      (cell) =>
        ["retrying", "stale"].includes(cell.state) ||
        (cell.state === "completed" && cell.classification !== "both_passed"),
    );
  }
  return row.cells.every((cell) => cell.state === "completed");
}

function ArmMark({ label, state }: { label: string; state: ArmState }) {
  return (
    <span className={`live-arm-mark ${state}`} aria-label={`${label}: ${state}`}>
      <span>{label}</span>
    </span>
  );
}

function formatInteger(value: number | null) {
  return value === null ? "—" : value.toLocaleString();
}

function formatRewards(rewards: Record<string, unknown>) {
  const entries = Object.entries(rewards);
  return entries.length ? entries.map(([key, value]) => `${key} ${String(value)}`).join(" · ") : "—";
}

function EvidenceBlock({ title, evidence, open = false }: { title: string; evidence: EvidenceText; open?: boolean }) {
  if (!evidence?.text) return null;
  return (
    <details className="live-evidence-block" open={open}>
      <summary>{title}{evidence.truncated ? " · truncated" : ""}</summary>
      <pre>{evidence.text}</pre>
    </details>
  );
}

function CaseArmPanel({ label, arm }: { label: "Nanocodex" | "Codex"; arm: CaseArm }) {
  const failed = arm.status !== "passed" || arm.outcome !== "passed";
  return (
    <section className={`live-case-arm ${failed ? "failed" : "passed"}`}>
      <header>
        <div>
          <span>{label}</span>
          <strong>{arm.status.replaceAll("_", " ")}</strong>
        </div>
        <span className="live-case-outcome">{arm.outcome.replaceAll("_", " ")}</span>
      </header>
      <dl className="live-case-metrics">
        <div><dt>Model</dt><dd>{arm.model ?? "—"}</dd></div>
        <div><dt>Agent time</dt><dd>{formatDuration(arm.durationMs)}</dd></div>
        <div><dt>Tool calls</dt><dd>{formatInteger(arm.toolCalls)}</dd></div>
        <div><dt>Total tokens</dt><dd>{formatInteger(arm.usage.totalTokens)}</dd></div>
        <div><dt>Cached input</dt><dd>{formatInteger(arm.usage.cachedInputTokens)}</dd></div>
        <div><dt>Guest memory</dt><dd>{formatPercent(arm.memory.guestUsedPercent)}</dd></div>
        <div><dt>Verifier</dt><dd>{arm.verifier.exitCode === null ? "—" : `exit ${arm.verifier.exitCode}`}</dd></div>
        <div><dt>Reward</dt><dd>{formatRewards(arm.verifier.rewards)}</dd></div>
      </dl>
      {arm.exception ? (
        <div className="live-case-exception">
          <AlertTriangle aria-hidden="true" />
          <div>
            <strong>{arm.exception.kind ?? "agent exception"}</strong>
            <p>{arm.exception.message ?? arm.exception.outcome ?? "No exception message retained."}</p>
          </div>
        </div>
      ) : null}
      <EvidenceBlock title="Verifier stdout" evidence={arm.verifier.stdout} open={failed} />
      <EvidenceBlock title="Verifier stderr" evidence={arm.verifier.stderr} open={failed} />
      {arm.finalMessage ? (
        <details className="live-evidence-block" open={failed && !arm.verifier.stdout?.text}>
          <summary>Final agent message</summary>
          <pre>{arm.finalMessage}</pre>
        </details>
      ) : null}
    </section>
  );
}

function CaseInspector({ row, cell, onClose }: { row: LiveRow; cell: LiveCell; onClose: () => void }) {
  const [detail, setDetail] = useState<CaseDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setDetail(null);
    setError(null);
    setLoading(Boolean(cell.detailId));
    if (!cell.detailId) return () => {
      active = false;
      controller.abort();
    };
    fetch(`/api/evals/case?id=${encodeURIComponent(cell.detailId)}`, {
      cache: "no-store",
      signal: controller.signal,
    })
      .then((response) => {
        if (!response.ok) throw new Error(`case detail returned ${response.status}`);
        return response.json() as Promise<CaseDetail>;
      })
      .then((next) => {
        if (active) setDetail(next);
      })
      .catch((reason) => {
        if (reason instanceof DOMException && reason.name === "AbortError") return;
        if (active) setError(reason instanceof Error ? reason.message : String(reason));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
      controller.abort();
    };
  }, [cell.detailId]);

  return (
    <article className={`live-case-detail ${cell.state}`} aria-label="Selected evaluation coordinate">
      <header>
        <div>
          <p className="eyebrow">{row.thinking} effort · {row.profileLabel}{cell.trial}</p>
          <h2>{row.taskLabel}</h2>
        </div>
        <button type="button" className="icon-button" onClick={onClose} aria-label="Close detail">
          <X aria-hidden="true" />
        </button>
      </header>
      <div className="live-case-detail-status">
        <strong>{cell.state}</strong>
        <span>{cell.classification?.replaceAll("_", " ") ?? "awaiting evidence"}</span>
        <span>{cell.attempts || 0} attempt{cell.attempts === 1 ? "" : "s"}</span>
        <span>{formatDuration(cell.durationMs)}</span>
      </div>
      {!cell.detailId ? (
        <div className="live-case-live">
          <section><strong>Nanocodex · {cell.nanocodexPhase?.phase ?? cell.nanocodex}</strong><p>{cell.nanocodexPhase?.summary ?? "No completed evidence yet."}</p></section>
          <section><strong>Codex · {cell.codexPhase?.phase ?? cell.codex}</strong><p>{cell.codexPhase?.summary ?? "No completed evidence yet."}</p></section>
        </div>
      ) : null}
      {loading ? <p className="live-case-loading">Loading retained case evidence…</p> : null}
      {error ? <p className="live-case-error"><AlertTriangle aria-hidden="true" /> {error}</p> : null}
      {detail ? (
        <>
          {detail.task.instruction ? (
            <section className="live-case-task">
              <p className="rail-label">Task instruction</p>
              <pre>{detail.task.instruction.text}</pre>
            </section>
          ) : null}
          <div className="live-case-arms">
            <CaseArmPanel label="Nanocodex" arm={detail.nanocodex} />
            <CaseArmPanel label="Codex" arm={detail.codex} />
          </div>
          {detail.history.length > 1 ? (
            <section className="live-case-history">
              <p className="rail-label">Attempt and replacement history</p>
              <ol>
                {detail.history.map((attempt, index) => (
                  <li key={`${attempt.trial}-${index}`}>
                    <strong>trial {attempt.trial}</strong>
                    <span>{attempt.classification.replaceAll("_", " ")}</span>
                    <span>N {attempt.nanocodexOutcome.replaceAll("_", " ")}</span>
                    <span>C {attempt.codexOutcome.replaceAll("_", " ")}</span>
                    <span>{formatDuration(attempt.durationMs)}</span>
                  </li>
                ))}
              </ol>
            </section>
          ) : null}
        </>
      ) : null}
    </article>
  );
}

export function LiveEvals({ connection }: { connection: LiveEvalConnection }) {
  const snapshot = connection.snapshot!;
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<MatrixFilter>("all");
  const [selected, setSelected] = useState<{ taskName: string; profile: string; trial: number } | null>(null);
  const detailRef = useRef<HTMLDivElement>(null);
  const normalizedQuery = query.trim().toLowerCase();
  const visibleRows = useMemo(
    () =>
      snapshot.rows.filter(
        (row) =>
          (!normalizedQuery || row.taskName.toLowerCase().includes(normalizedQuery)) &&
          rowMatchesFilter(row, filter),
      ),
    [filter, normalizedQuery, snapshot.rows],
  );
  const selectedCase = useMemo(() => {
    if (!selected) return null;
    const row = snapshot.rows.find(
      (candidate) => candidate.taskName === selected.taskName && candidate.profile === selected.profile,
    );
    const cell = row?.cells.find((candidate) => candidate.trial === selected.trial);
    return row && cell ? { row, cell } : null;
  }, [selected, snapshot.rows]);

  useEffect(() => {
    if (!selectedCase) return;
    window.requestAnimationFrame(() =>
      detailRef.current?.scrollIntoView({ behavior: "smooth", block: "start" }),
    );
  }, [selectedCase?.row.taskName, selectedCase?.row.profile, selectedCase?.cell.trial]);
  const completionPercent = snapshot.summary.total
    ? (snapshot.summary.completed / snapshot.summary.total) * 100
    : 0;
  const trialCount = Math.max(0, ...snapshot.rows.map((row) => row.cells.length));
  const host = snapshot.health.host;

  return (
    <div className="live-evals">
      <section className="live-evals-hero page-grid">
        <div>
          <p className="eyebrow"><Radio aria-hidden="true" /> Live retained evidence</p>
          <h1>Evals</h1>
          <p>
            Every task, treatment, and repetition in one durable matrix. Cells update as model,
            tool, verifier, retry, and completion evidence lands on disk.
          </p>
        </div>
        <div className={`live-health-callout ${snapshot.health.status}`}>
          <span className="live-health-pulse" />
          <div>
            <strong>{snapshot.health.status}</strong>
            <p>{snapshot.health.statusMessage}</p>
          </div>
          <small>{connection.connected ? "streaming" : "reconnecting"} · {formatAge(snapshot.health.evidenceAgeMs)}</small>
        </div>
      </section>

      <section className="live-health-strip page-grid" aria-label="Evaluation system health">
        <div>
          <Activity aria-hidden="true" />
          <span>Throughput</span>
          <strong>{snapshot.health.completions5m}</strong>
          <small>attempts / 5m</small>
        </div>
        <div>
          <Gauge aria-hidden="true" />
          <span>Active</span>
          <strong>{snapshot.summary.running}</strong>
          <small>{snapshot.summary.stale} stale</small>
        </div>
        <div>
          <Cpu aria-hidden="true" />
          <span>Host CPU</span>
          <strong>{host.cpuPercent === null ? "warming" : formatPercent(host.cpuPercent)}</strong>
          <small>normalized load {formatPercent(host.loadPercent)}</small>
        </div>
        <div>
          <MemoryStick aria-hidden="true" />
          <span>Memory</span>
          <strong>{formatPercent(host.memoryUsedPercent)}</strong>
          <small>{availablePercent(host.memoryUsedPercent)}</small>
        </div>
        <div>
          <Server aria-hidden="true" />
          <span>Swap</span>
          <strong>{formatPercent(host.swapUsedPercent)}</strong>
          <small>utilization</small>
        </div>
        <div>
          <HardDrive aria-hidden="true" />
          <span>Eval disk</span>
          <strong>{formatPercent(host.diskUsedPercent)}</strong>
          <small>{availablePercent(host.diskUsedPercent)}</small>
        </div>
      </section>

      <section className="live-progress page-grid" aria-label="Sweep progress">
        <header>
          <div>
            <span>Valid matched coordinates</span>
            <strong>{snapshot.summary.completed.toLocaleString()} / {snapshot.summary.total.toLocaleString()}</strong>
          </div>
          <div className="live-progress-counts">
            <span><i className="running" /> {snapshot.summary.running} running</span>
            <span><i className="retrying" /> {snapshot.summary.retrying} retrying</span>
            <span><i className="queued" /> {snapshot.summary.queued} queued</span>
          </div>
        </header>
        <div className="live-progress-track"><span style={{ width: `${completionPercent}%` }} /></div>
      </section>

      <section className="live-matrix-section page-grid">
        <header className="live-matrix-toolbar">
          <div>
            <p className="rail-label">CM / CMO · k={trialCount}</p>
            <h2>Task matrix</h2>
          </div>
          <label className="live-eval-search">
            <Search aria-hidden="true" />
            <input
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Filter tasks"
              aria-label="Filter evaluation tasks"
            />
          </label>
          <div className="live-filter" role="group" aria-label="Matrix filter">
            {(["all", "active", "issues", "complete"] as MatrixFilter[]).map((option) => (
              <button
                type="button"
                className={filter === option ? "is-active" : ""}
                onClick={() => setFilter(option)}
                key={option}
              >
                {option}
              </button>
            ))}
          </div>
        </header>

        <div className="live-matrix-legend" aria-label="Matrix legend">
          <span><i className="both" /> both passed</span>
          <span><i className="nano" /> Nanocodex only</span>
          <span><i className="codex" /> Codex only</span>
          <span><i className="neither" /> neither passed</span>
          <span><i className="running" /> running</span>
          <span><i className="retrying" /> replacement</span>
        </div>

        {selectedCase ? (
          <div className="live-case-slot" ref={detailRef}>
            <CaseInspector
              row={selectedCase.row}
              cell={selectedCase.cell}
              onClose={() => setSelected(null)}
            />
          </div>
        ) : null}

        <div className="live-matrix-scroll">
          <table className="live-matrix">
            <thead>
              <tr>
                <th>Task</th>
                <th>Mode</th>
                {Array.from({ length: trialCount }, (_, index) => <th key={index}>#{index + 1}</th>)}
                <th>Done</th>
              </tr>
            </thead>
            <tbody>
              {visibleRows.map((row, index) => {
                const firstTaskRow = index === 0 || visibleRows[index - 1].taskName !== row.taskName;
                const taskRowCount = visibleRows.filter((candidate) => candidate.taskName === row.taskName).length;
                return (
                  <tr key={`${row.taskName}-${row.profile}`} className={!firstTaskRow ? "same-task" : ""}>
                    {firstTaskRow ? <th rowSpan={taskRowCount} scope="rowgroup">{row.taskLabel}</th> : null}
                    <th scope="row"><span>{row.profileLabel}</span><small>{row.thinking}</small></th>
                    {row.cells.map((cell) => (
                      <td key={cell.trial}>
                        <button
                          type="button"
                          className={`live-matrix-cell ${cell.state} ${cell.classification ?? ""}`}
                          title={cellTitle(row, cell)}
                          aria-pressed={
                            selected?.taskName === row.taskName &&
                            selected.profile === row.profile &&
                            selected.trial === cell.trial
                          }
                          onClick={() => setSelected({
                            taskName: row.taskName,
                            profile: row.profile,
                            trial: cell.trial,
                          })}
                        >
                          <span className="live-cell-number">{cell.trial}</span>
                          <span className="live-cell-arms">
                            <ArmMark label="N" state={cell.nanocodex} />
                            <ArmMark label="C" state={cell.codex} />
                          </span>
                          {cell.attempts > 1 ? <span className="live-attempt-count"><RotateCw aria-hidden="true" />{cell.attempts}</span> : null}
                        </button>
                      </td>
                    ))}
                    <td className="live-row-total">
                      {row.cells.filter((cell) => cell.state === "completed").length}/{row.cells.length}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </section>

      <section className="live-eval-footer page-grid">
        <div>
          <CheckCircle2 aria-hidden="true" />
          <span>Both passed</span>
          <strong>{snapshot.summary.bothPassed}</strong>
        </div>
        <div>
          <CircleDashed aria-hidden="true" />
          <span>Different outcomes</span>
          <strong>{snapshot.summary.nanocodexOnly + snapshot.summary.codexOnly}</strong>
        </div>
        <div>
          <AlertTriangle aria-hidden="true" />
          <span>Infrastructure · 15m</span>
          <strong>{snapshot.health.infrastructure15m}</strong>
        </div>
        <div>
          <Radio aria-hidden="true" />
          <span>Evidence sources</span>
          <strong>{snapshot.sourceCount}</strong>
        </div>
      </section>
    </div>
  );
}
