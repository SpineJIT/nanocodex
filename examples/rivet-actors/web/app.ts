import { createClient } from "rivetkit/client";

import type { registry } from "../src/registry.js";

type PendingTurn = { id: string; input: string };
type Message = { role: "you" | "agent" | "error"; text: string };
type WebState = {
  endpoint: string;
  actorKey: string;
  pending?: PendingTurn;
  messages: Message[];
};
const STORAGE_KEY = "nanocodex.rivet.web.v1";
const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const ui = {
  activity: byId<HTMLSpanElement>("activity"),
  actor: byId<HTMLElement>("actor"),
  actorKey: byId<HTMLInputElement>("actor-key"),
  connect: byId<HTMLButtonElement>("connect"),
  detach: byId<HTMLButtonElement>("detach"),
  endpoint: byId<HTMLInputElement>("endpoint"),
  form: byId<HTMLFormElement>("prompt-form"),
  input: byId<HTMLTextAreaElement>("prompt"),
  newActor: byId<HTMLButtonElement>("new-actor"),
  send: byId<HTMLButtonElement>("send"),
  status: byId<HTMLSpanElement>("status"),
  transcript: byId<HTMLElement>("transcript"),
};
const makeClient = (endpoint: string) => createClient<typeof registry>({ endpoint });
type NanocodexClient = ReturnType<typeof makeClient>;
type Session = ReturnType<NanocodexClient["nanocodex"]["getOrCreate"]>;
type Connection = ReturnType<Session["connect"]>;

let state = loadState() ?? {
  endpoint: "http://127.0.0.1:6420",
  actorKey: crypto.randomUUID(),
  messages: [],
};
let client: NanocodexClient | undefined;
let connection: Connection | undefined;
let generation = 0;
let eventCount = 0;

ui.endpoint.value = state.endpoint;
ui.actorKey.value = state.actorKey;
renderMessages();
saveState();
void connect();

ui.connect.addEventListener("click", () => void connect());
ui.newActor.addEventListener("click", () => {
  void detach();
  state = {
    endpoint: ui.endpoint.value.trim() || "http://127.0.0.1:6420",
    actorKey: crypto.randomUUID(),
    messages: [],
  };
  ui.actorKey.value = state.actorKey;
  ui.actor.textContent = "not resolved";
  saveState();
  renderMessages();
  void connect();
});
ui.detach.addEventListener("click", () => void detach());
ui.form.addEventListener("submit", (event) => {
  event.preventDefault();
  const input = ui.input.value.trim();
  if (!input) return setActivity("prompt is empty", true);
  if (!connection) return setActivity("connect to the actor first", true);
  if (state.pending) return setActivity("one durable turn is already pending", true);
  state.pending = { id: crypto.randomUUID(), input };
  state.messages.push({ role: "you", text: input });
  ui.input.value = "";
  saveState();
  renderMessages();
  void runPending(connection, generation);
});

async function connect(): Promise<void> {
  const endpoint = ui.endpoint.value.trim();
  const actorKey = ui.actorKey.value.trim();
  if (!endpoint || !actorKey) return setActivity("endpoint and actor key are required", true);
  await detach(false);
  state.endpoint = endpoint.replace(/\/$/, "");
  state.actorKey = actorKey;
  saveState();
  setStatus("connecting", "warn");
  const activeGeneration = ++generation;
  try {
    client = makeClient(state.endpoint);
    const handle = client.nanocodex.getOrCreate([state.actorKey]);
    const nextConnection = handle.connect();
    connection = nextConnection;
    nextConnection.on("agentEvent", (event) => {
      if (activeGeneration !== generation) return;
      eventCount += 1;
      const kind = event && typeof event === "object" && "type" in event ? String(event.type) : "agent event";
      setActivity(kind + " · " + eventCount + " events");
    });
    nextConnection.onStatusChange((status) => {
      if (activeGeneration === generation) setStatus(status, status === "connected" ? "ok" : "warn");
    });
    await nextConnection.ready;
    const [actorId, sessionStatus] = await Promise.all([handle.resolve(), nextConnection.status()]);
    if (activeGeneration !== generation) return;
    ui.actor.textContent = actorId;
    setStatus("ready", "ok");
    setActivity(sessionStatus.completed_turns + " committed turns");
    if (state.pending) void runPending(nextConnection, activeGeneration);
  } catch (error) {
    if (activeGeneration === generation) {
      setStatus("failed", "bad");
      setActivity(errorMessage(error), true);
    }
  }
}

async function runPending(active: Connection, activeGeneration: number): Promise<void> {
  const pending = state.pending;
  if (!pending) return;
  try {
    const accepted = await active.start(pending);
    if (activeGeneration !== generation) return;
    setStatus(accepted.replayed ? "resuming" : "running", "ok");
    setActivity((accepted.replayed ? "rejoined " : "started ") + shortId(pending.id) + "; detach any time");
    const completed = await active.prompt(pending);
    if (activeGeneration !== generation || state.pending?.id !== completed.id) return;
    state.messages.push({ role: "agent", text: completed.final_message });
    delete state.pending;
    saveState();
    renderMessages();
    setStatus("ready", "ok");
    setActivity("committed durably · " + eventCount + " events");
  } catch (error) {
    if (activeGeneration !== generation) return;
    state.messages.push({ role: "error", text: errorMessage(error) });
    delete state.pending;
    saveState();
    renderMessages();
    setStatus("failed", "bad");
    setActivity(errorMessage(error), true);
  }
}

async function detach(showMessage = true): Promise<void> {
  generation += 1;
  const oldConnection = connection;
  const oldClient = client;
  connection = undefined;
  client = undefined;
  await Promise.allSettled([oldConnection?.dispose(), oldClient?.dispose()]);
  if (showMessage) {
    setStatus(state.pending ? "detached · resumable" : "detached", "warn");
    setActivity("safe to close; inference and state remain with the actor");
  }
}

function renderMessages(): void {
  ui.transcript.replaceChildren();
  if (!state.messages.length) {
    const empty = document.createElement("article");
    empty.className = "system";
    empty.textContent = "Send a prompt, detach during inference, then reload to rejoin the idempotent turn.";
    ui.transcript.append(empty);
  }
  for (const message of state.messages) {
    const article = document.createElement("article");
    article.className = message.role;
    const label = document.createElement("strong");
    label.textContent = message.role;
    const text = document.createElement("div");
    text.textContent = message.text;
    article.append(label, text);
    ui.transcript.append(article);
  }
  ui.transcript.scrollTop = ui.transcript.scrollHeight;
}

function loadState(): WebState | undefined {
  try {
    const value = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null") as Partial<WebState> | null;
    if (!value || typeof value.endpoint !== "string" || typeof value.actorKey !== "string") return undefined;
    return {
      endpoint: value.endpoint,
      actorKey: value.actorKey,
      messages: Array.isArray(value.messages) ? value.messages.slice(-50) : [],
      ...(value.pending && typeof value.pending.id === "string" && typeof value.pending.input === "string"
        ? { pending: value.pending }
        : {}),
    };
  } catch {
    return undefined;
  }
}

function saveState(): void {
  state.messages = state.messages.slice(-50);
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    state.messages = state.messages.slice(-10).map((message) => ({
      ...message,
      text: message.text.slice(-20_000),
    }));
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  }
}

function setStatus(text: string, tone?: "ok" | "warn" | "bad"): void {
  ui.status.textContent = text;
  ui.status.dataset.tone = tone ?? "";
}
function setActivity(text: string, bad = false): void {
  ui.activity.textContent = text;
  ui.activity.dataset.bad = bad ? "true" : "false";
}
function shortId(id: string): string { return id.slice(0, 8); }
function errorMessage(error: unknown): string { return error instanceof Error ? error.message : String(error); }
