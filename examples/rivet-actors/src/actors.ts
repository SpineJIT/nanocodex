import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

import { agentOS } from "@rivet-dev/agentos";
import type {
  AgentEvent,
  DefaultAgent,
  EventWatcher,
  SessionSnapshot,
  Turn,
  TurnUsage,
} from "nanocodex";
import { Agent } from "nanocodex/browser";
import { actor, event, UserError } from "rivetkit";
import { db, type RawAccess } from "rivetkit/db";

import type { SubscriptionSnapshot, SubscriptionStatus } from "./auth.js";
import { type AuthCapabilityProof, createCapabilityProof } from "./auth-capability.js";
import {
  type BrowserAuthRequest,
  openApiKeyWebSocket,
  openSubscriptionWebSocket,
} from "./model-websocket.js";

const MAX_PROMPT_BYTES = 1024 * 1024;
const MAX_ACTIVE_TURNS = 16;
const MAX_TERMINAL_TURNS = 256;
const TURN_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;
const CHATGPT_WEBSOCKET_URL = "wss://chatgpt.com/backend-api/codex/responses";
const CHATGPT_API_BASE_URL = "https://chatgpt.com/backend-api/codex";
const wasmBytes = readFile(new URL(import.meta.resolve("nanocodex/wasm")));

type ModelAuthMode = "api_key" | "chatgpt";

type InFlightTurn = {
  inputHash: string;
  operation: Promise<TurnCompleted>;
};

type SessionVars = {
  agent: DefaultAgent | undefined;
  agentPromise: Promise<DefaultAgent> | undefined;
  events: EventWatcher | undefined;
  inFlight: Map<string, InFlightTurn>;
  turns: Map<string, Turn>;
};

export type PromptRequest = {
  id: string;
  input: string;
};

export type TurnCompleted = {
  type: "turn_completed";
  id: string;
  final_message: string;
  usage: TurnUsage;
};

export type TurnFailed = {
  type: "turn_failed";
  id: string;
  error: string;
};

export type SessionStatus = {
  session_id: string;
  has_snapshot: boolean;
  completed_turns: number;
  last_active: number;
  active_turns: string[];
  agent_loaded: boolean;
  auth_mode: ModelAuthMode;
};

type SessionRow = {
  snapshot: string | null;
  completed_turns: number;
  last_active: number;
};

type TerminalRow = {
  input_hash: string;
  payload: string;
};

type AuthActorHandle = {
  snapshot(proof: AuthCapabilityProof): Promise<SubscriptionSnapshot>;
  recover(proof: AuthCapabilityProof, revision: number): Promise<SubscriptionSnapshot>;
  status(proof: AuthCapabilityProof): Promise<SubscriptionStatus>;
};

type InternalActorClient = {
  nanocodexAuth: {
    getOrCreate(key: string[]): AuthActorHandle;
  };
  nanocodex: {
    getOrCreate(key: string[]): {
      prompt(request: PromptRequest): Promise<TurnCompleted>;
    };
  };
};

type SessionContext = {
  vars: SessionVars;
  db: RawAccess;
  actorId: string;
  abortSignal: AbortSignal;
  broadcast(name: string, payload: unknown): void;
  client(): unknown;
  keepAwake<T>(promise: Promise<T>): Promise<T>;
};

type WorkspaceContext = {
  state: { delegatedTurns: number };
  client(): unknown;
};

export const nanocodex = actor({
  state: {},
  createVars: (): SessionVars => ({
    agent: undefined,
    agentPromise: undefined,
    events: undefined,
    inFlight: new Map(),
    turns: new Map(),
  }),
  db: db({
    onMigrate: async (database) => {
      await database.execute(`
        CREATE TABLE IF NOT EXISTS session_state (
          singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
          snapshot TEXT,
          completed_turns INTEGER NOT NULL DEFAULT 0,
          last_active INTEGER NOT NULL DEFAULT 0
        )
      `);
      await database.execute(`
        CREATE TABLE IF NOT EXISTS terminal_turns (
          id TEXT PRIMARY KEY,
          input_hash TEXT NOT NULL,
          payload TEXT NOT NULL,
          completed_at INTEGER NOT NULL
        )
      `);
      await database.execute(
        "INSERT OR IGNORE INTO session_state (singleton, last_active) VALUES (1, 0)",
      );
    },
  }),
  events: {
    agentEvent: event<AgentEvent>(),
    turnCompleted: event<TurnCompleted>(),
    turnFailed: event<TurnFailed>(),
  },
  actions: {
    prompt: async (c, request: PromptRequest): Promise<TurnCompleted> => {
      validatePrompt(request);
      const inputHash = hashInput(request.input);
      const existing = c.vars.inFlight.get(request.id);
      if (existing) {
        if (existing.inputHash !== inputHash) throw userError(`turn ${request.id} already has different input`);
        return c.keepAwake(existing.operation);
      }
      if (c.vars.inFlight.size >= MAX_ACTIVE_TURNS) {
        throw userError(`at most ${MAX_ACTIVE_TURNS} turns may be active`);
      }

      const operation = runPrompt(c, request, inputHash);
      const inFlight = { inputHash, operation };
      c.vars.inFlight.set(request.id, inFlight);
      try {
        return await c.keepAwake(operation);
      } finally {
        if (c.vars.inFlight.get(request.id) === inFlight) c.vars.inFlight.delete(request.id);
      }
    },
    steer: async (c, id: string, input: string): Promise<void> => {
      validateTurnId(id);
      validateInput(input);
      const turn = c.vars.turns.get(id);
      if (!turn) throw userError(`turn ${id} is not active`);
      await turn.steer({ input });
    },
    cancel: async (c, id: string): Promise<void> => {
      validateTurnId(id);
      const turn = c.vars.turns.get(id);
      if (!turn) throw userError(`turn ${id} is not active`);
      await turn.cancel();
    },
    status: async (c): Promise<SessionStatus> => {
      const row = await sessionRow(c.db);
      return {
        session_id: sessionUuid(c.actorId),
        has_snapshot: row.snapshot !== null,
        completed_turns: row.completed_turns,
        last_active: row.last_active,
        active_turns: [...c.vars.inFlight.keys()],
        agent_loaded: c.vars.agent !== undefined,
        auth_mode: modelAuthMode(),
      };
    },
    unload: async (c): Promise<void> => {
      if (c.vars.inFlight.size > 0) throw userError("cannot unload while turns are active");
      await shutdown(c);
    },
    reset: async (c): Promise<void> => {
      if (c.vars.inFlight.size > 0) throw userError("cannot reset while turns are active");
      await shutdown(c);
      await c.db.transaction(async (transaction) => {
        await transaction.execute("DELETE FROM terminal_turns");
        await transaction.execute(
          "UPDATE session_state SET snapshot = NULL, completed_turns = 0, last_active = 0 WHERE singleton = 1",
        );
      });
    },
  },
  onSleep: shutdown,
  onDestroy: shutdown,
  options: {
    actionTimeout: 10 * 60_000,
    sleepGracePeriod: 30_000,
    sleepTimeout: 30_000,
  },
});

// AgentOS actors and ordinary Rivet Actors share one registry and actor-to-actor
// client. This actor keeps AgentOS filesystem/process/session capabilities while
// delegating model work to the lighter Nanocodex actor instead of nesting one
// complete harness inside another ACP VM.
export const nanocodexWorkspace = agentOS({
  defaultSoftware: false,
  software: [],
  state: { delegatedTurns: 0 },
  actions: {
    nanocodex: {
      prompt: async (
        c: WorkspaceContext,
        sessionKey: string,
        request: PromptRequest,
      ): Promise<TurnCompleted> => {
        validateSessionKey(sessionKey);
        const client = c.client() as InternalActorClient;
        const peer = client.nanocodex.getOrCreate([sessionKey]);
        const result = await peer.prompt(request);
        c.state.delegatedTurns += 1;
        return result;
      },
    },
  },
});

async function runPrompt(
  context: SessionContext,
  request: PromptRequest,
  inputHash: string,
): Promise<TurnCompleted> {
  const stored = await terminal(context.db, request.id);
  if (stored) {
    if (stored.input_hash !== inputHash) throw userError(`turn ${request.id} already has different input`);
    return parseTerminal(stored.payload);
  }

  let turn: Turn | undefined;
  const onAbort = () => {
    void turn?.cancel().catch(() => {});
  };
  context.abortSignal.addEventListener("abort", onAbort, { once: true });
  try {
    const agent = await ensureAgent(context);
    turn = agent.turn.prompt({ input: request.input });
    context.vars.turns.set(request.id, turn);
    const result = await turn.result();
    const completed: TurnCompleted = {
      type: "turn_completed",
      id: request.id,
      final_message: result.finalMessage,
      usage: result.usage,
    };
    const payload = JSON.stringify(completed);
    const snapshot = JSON.stringify(result.snapshot);
    const completedAt = Date.now();
    try {
      await context.db.transaction(async (transaction) => {
        await transaction.execute(
          `UPDATE session_state
           SET snapshot = ?, completed_turns = completed_turns + 1, last_active = ?
           WHERE singleton = 1`,
          snapshot,
          completedAt,
        );
        await transaction.execute(
          `INSERT INTO terminal_turns (id, input_hash, payload, completed_at)
           VALUES (?, ?, ?, ?)
           ON CONFLICT(id) DO UPDATE SET
             input_hash = excluded.input_hash,
             payload = excluded.payload,
             completed_at = excluded.completed_at`,
          request.id,
          inputHash,
          payload,
          completedAt,
        );
        await transaction.execute(
          `DELETE FROM terminal_turns
           WHERE id NOT IN (
             SELECT id FROM terminal_turns
             ORDER BY completed_at DESC, rowid DESC LIMIT ?
           )`,
          MAX_TERMINAL_TURNS,
        );
      });
    } catch (error) {
      await shutdown(context);
      throw new Error(`durable commit failed: ${errorMessage(error)}`);
    }
    context.broadcast("turnCompleted", completed);
    return completed;
  } catch (error) {
    const failed: TurnFailed = {
      type: "turn_failed",
      id: request.id,
      error: errorMessage(error),
    };
    context.broadcast("turnFailed", failed);
    if (error instanceof UserError) throw error;
    throw userError(failed.error, "turn_failed");
  } finally {
    context.abortSignal.removeEventListener("abort", onAbort);
    context.vars.turns.delete(request.id);
    turn?.dispose();
  }
}

async function ensureAgent(context: SessionContext): Promise<DefaultAgent> {
  if (context.vars.agent) return context.vars.agent;
  if (context.vars.agentPromise) return context.vars.agentPromise;
  const promise = createAgent(context);
  context.vars.agentPromise = promise;
  try {
    context.vars.agent = await promise;
    return context.vars.agent;
  } finally {
    if (context.vars.agentPromise === promise) context.vars.agentPromise = undefined;
  }
}

async function createAgent(context: SessionContext): Promise<DefaultAgent> {
  const row = await sessionRow(context.db);
  const resume = row.snapshot === null ? undefined : JSON.parse(row.snapshot) as SessionSnapshot;
  const mode = modelAuthMode();
  const common = {
    apiBaseUrl: mode === "chatgpt" ? CHATGPT_API_BASE_URL : undefined,
    instructions: "You are Nanocodex running as a durable Rivet Actor alongside AgentOS.",
    module: await wasmBytes,
    resume,
    sessionId: sessionUuid(context.actorId),
    tools: {
      runtimeInfo: {
        description: "Return information about the current agent runtime.",
        parameters: { type: "object", additionalProperties: false },
        handler: () => ({ runtime: "rivet-actor", session_id: sessionUuid(context.actorId) }),
      },
    },
    websocketUrl: process.env.OPENAI_WEBSOCKET_URL
      ?? (mode === "chatgpt" ? CHATGPT_WEBSOCKET_URL : undefined),
    workspace: "/workspace",
  };

  let agent: DefaultAgent;
  if (mode === "api_key") {
    const apiKey = requiredSecret("OPENAI_API_KEY");
    agent = await Agent.create({
      ...common,
      apiKey,
      createWebSocket: openApiKeyWebSocket,
    });
  } else {
    const client = context.client() as InternalActorClient;
    const auth = client.nanocodexAuth.getOrCreate([
      process.env.NANOCODEX_AUTH_ACTOR_KEY ?? "subscription",
    ]);
    agent = await Agent.create({
      ...common,
      hostAuth: true,
      createWebSocket: (endpoint: string, sessionId: string, request: BrowserAuthRequest) =>
        openSubscriptionWebSocket({
          snapshot: () => auth.snapshot(createCapabilityProof("snapshot")),
          recover: (revision) => auth.recover(
            createCapabilityProof(`recover:${revision}`),
            revision,
          ),
        }, endpoint, sessionId, request),
    });
  }
  const watcher = agent.events.watch();
  watcher.onEvent((agentEvent) => context.broadcast("agentEvent", agentEvent));
  context.vars.events = watcher;
  return agent;
}

async function shutdown(context: SessionContext): Promise<void> {
  const turns = [...context.vars.turns.values()];
  await Promise.all(turns.map(async (turn) => {
    try {
      await turn.cancel();
    } catch {
      // A terminal turn needs no cancellation.
    }
  }));
  context.vars.turns.clear();
  context.vars.events?.off();
  context.vars.events = undefined;

  let agent = context.vars.agent;
  if (!agent && context.vars.agentPromise) {
    try {
      agent = await context.vars.agentPromise;
    } catch {
      return;
    }
  }
  context.vars.agent = undefined;
  context.vars.agentPromise = undefined;
  if (!agent) return;
  try {
    await agent.session.shutdown();
  } finally {
    agent.dispose();
  }
}

async function sessionRow(database: RawAccess): Promise<SessionRow> {
  const row = (await database.execute<SessionRow>(
    "SELECT snapshot, completed_turns, last_active FROM session_state WHERE singleton = 1",
  ))[0];
  if (!row) throw new Error("session state is not initialized");
  return row;
}

async function terminal(database: RawAccess, id: string): Promise<TerminalRow | undefined> {
  return (await database.execute<TerminalRow>(
    "SELECT input_hash, payload FROM terminal_turns WHERE id = ?",
    id,
  ))[0];
}

function parseTerminal(payload: string): TurnCompleted {
  const parsed = JSON.parse(payload) as TurnCompleted;
  if (parsed.type !== "turn_completed" || typeof parsed.id !== "string") {
    throw new Error("stored terminal result is invalid");
  }
  return parsed;
}

function validatePrompt(request: PromptRequest): void {
  if (!request || typeof request !== "object") throw userError("prompt request must be an object");
  validateTurnId(request.id);
  validateInput(request.input);
}

function validateTurnId(id: string): void {
  if (typeof id !== "string" || !TURN_ID.test(id)) {
    throw userError("turn id must be 1-128 safe ASCII characters");
  }
}

function validateInput(input: string): void {
  if (typeof input !== "string" || input.length === 0) throw userError("prompt input must be a non-empty string");
  if (Buffer.byteLength(input, "utf8") > MAX_PROMPT_BYTES) {
    throw userError("prompt input exceeds 1 MiB");
  }
}

function validateSessionKey(key: string): void {
  if (typeof key !== "string" || key.length === 0 || key.length > 128) {
    throw userError("session key must be 1-128 characters");
  }
}

function hashInput(input: string): string {
  return createHash("sha256").update(input).digest("hex");
}

function sessionUuid(actorId: string): string {
  const hex = createHash("sha256").update(`nanocodex:rivet:${actorId}`).digest("hex").slice(0, 32);
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-7${hex.slice(13, 16)}-8${hex.slice(17, 20)}-${hex.slice(20)}`;
}

function modelAuthMode(): ModelAuthMode {
  const configured = process.env.NANOCODEX_AUTH_MODE ?? "api_key";
  if (configured === "api_key" || configured === "chatgpt") return configured;
  throw new Error("NANOCODEX_AUTH_MODE must be api_key or chatgpt");
}

function requiredSecret(name: string): string {
  const value = process.env[name];
  if (!value?.trim()) throw new Error(`${name} is not configured`);
  return value;
}

function errorMessage(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  return message.length <= 1_024 ? message : `${message.slice(0, 1_021)}...`;
}

function userError(message: string, code = "invalid_request"): UserError {
  return new UserError(message, { code });
}
