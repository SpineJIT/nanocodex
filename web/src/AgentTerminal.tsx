import { lazy, Suspense, useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import {
  NanocodexProvider,
  useNanocodex,
  useNanocodexMessage,
} from "nanocodex-react";
import { NanocodexTui } from "nanocodex-tui-react";
import "nanocodex-tui-react/structure.css";

import {
  nanocodexConfig,
  type AgentTransport,
  type PaymentStatus,
  type WebTuiCommand,
  type WebTuiMessage,
} from "./nanocodex";
import type { TempoAccessKey } from "./tempoAccessKey";

const MppControls = lazy(async () => ({
  default: (await import("./MppControls")).MppControls,
}));

/** Website policy around the reusable TUI: credential UX and the site theme. */
export function AgentTerminal() {
  return (
    <NanocodexProvider config={nanocodexConfig}>
      <AgentTerminalDemo />
    </NanocodexProvider>
  );
}

function AgentTerminalDemo() {
  const agent = useNanocodex<WebTuiCommand>();
  const [transport, setTransport] = useState<AgentTransport>("openai");
  const [credentialSource, setCredentialSource] = useState<CredentialSource | undefined>();
  const [payment, setPayment] = useState<PaymentStatus>();
  const [jsonl, setJsonl] = useState<string[]>([]);
  useNanocodexMessage<WebTuiMessage>((message) => {
    if (message.type === "mppPayment") setPayment(message.payment);
    if (message.type === "mppJsonl") {
      setJsonl((current) => [...current.slice(-99), message.line]);
    }
  });
  useEffect(() => {
    let active = true;
    void fetch("/api/health")
      .then(async (response) => {
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return response.json() as Promise<{
          agent_configured?: boolean;
          credential_source?: CredentialSource;
        }>;
      })
      .then((health) => {
        if (!active) return;
        setCredentialSource(health.agent_configured === true
          && (health.credential_source === "user" || health.credential_source === "deployment")
          ? health.credential_source
          : null);
      }, () => {
        if (active) setCredentialSource(null);
      });
    return () => { active = false; };
  }, []);

  useEffect(() => {
    setPayment(undefined);
    setJsonl([]);
    if (transport !== "openai") return;
    if (credentialSource === "user" || credentialSource === "deployment") {
      nanocodexConfig.restart(startCommand("openai"));
    } else {
      nanocodexConfig.disconnect();
    }
  }, [credentialSource, transport]);

  const startMpp = useCallback((key: TempoAccessKey) => {
    nanocodexConfig.restart(startCommand("mpp", key));
  }, []);
  const disconnectMpp = useCallback(() => nanocodexConfig.disconnect(), []);
  const selectTransport = (next: AgentTransport) => {
    if (next === transport) return;
    nanocodexConfig.disconnect();
    setTransport(next);
  };

  const enabled = transport === "openai"
    ? credentialSource === "user" || credentialSource === "deployment"
    : agent.status === "ready";
  const unavailableMessage = transport === "openai"
    ? credentialSource === undefined
      ? "Checking OpenAI credentials..."
      : "OpenAI API key is not configured"
    : agent.status === "starting"
      ? "Starting paid MPP session..."
      : agent.status === "error"
        ? agent.error ?? "MPP session failed"
        : "Connect Tempo to authorize an MPP session";

  return (
    <div className="nanocodex-demo">
      <div className="agent-transport" role="group" aria-label="Agent connection">
        <button
          type="button"
          aria-pressed={transport === "openai"}
          onClick={() => selectTransport("openai")}
        >OpenAI API</button>
        <button
          type="button"
          aria-pressed={transport === "mpp"}
          onClick={() => selectTransport("mpp")}
        >Tempo MPP</button>
      </div>
      {transport === "openai" ? <CredentialBar source={credentialSource} /> : (
        <Suspense fallback={<aside className="agent-byok">Loading Tempo Accounts…</aside>}>
          <MppControls
            jsonl={jsonl}
            payment={payment}
            onDisconnect={disconnectMpp}
            onReady={startMpp}
          />
        </Suspense>
      )}
      <NanocodexTui
        key={transport}
        enabled={enabled}
        unavailableMessage={unavailableMessage}
      />
    </div>
  );
}

function startCommand(transport: "openai"): WebTuiCommand;
function startCommand(transport: "mpp", paymentKey: TempoAccessKey): WebTuiCommand;
function startCommand(transport: AgentTransport, paymentKey?: TempoAccessKey): WebTuiCommand {
  if (transport === "mpp") {
    if (!paymentKey) throw new Error("MPP requires an authorized Tempo access key");
    return {
      type: "start",
      transport,
      paymentKey,
      thinking: "none",
      reasoningMode: "standard",
    };
  }
  return {
    type: "start",
    transport,
    thinking: "high",
    reasoningMode: "standard",
  };
}

type CredentialSource = "user" | "deployment" | null;

function CredentialBar({ source }: { source: CredentialSource | undefined }) {
  const [editing, setEditing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const keyRef = useRef<HTMLInputElement>(null);

  const save = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const apiKey = keyRef.current?.value.trim() ?? "";
    if (!apiKey) {
      setError("Enter an OpenAI API key.");
      keyRef.current?.focus();
      return;
    }
    setBusy(true);
    setError("");
    try {
      const response = await fetch("/api/auth/openai", {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ api_key: apiKey }),
      });
      if (keyRef.current) keyRef.current.value = "";
      if (!response.ok) throw new Error(await credentialError(response));
      window.location.reload();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Could not start the key session.");
      setBusy(false);
    }
  };

  const clear = async () => {
    setBusy(true);
    setError("");
    try {
      const response = await fetch("/api/auth/openai", {
        method: "DELETE",
        credentials: "same-origin",
      });
      if (!response.ok) throw new Error(await credentialError(response));
      window.location.reload();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Could not forget the key session.");
      setBusy(false);
    }
  };

  const label = source === undefined
    ? "Checking OpenAI credentials"
    : source === "user"
    ? "Using your OpenAI API key"
    : source === "deployment"
      ? "Using the site demo key"
      : "Add an OpenAI API key to run the agent";

  return (
    <aside className="agent-byok" aria-label="OpenAI API key">
      <div className="agent-byok-summary">
        <span><i className={source ? "is-ready" : ""} aria-hidden="true" />{label}</span>
        <div>
          <button type="button" onClick={() => { setEditing((value) => !value); setError(""); }} disabled={busy}>
            {source === "user" ? "Replace key" : "Use your key"}
          </button>
          {source === "user" ? <button type="button" onClick={clear} disabled={busy}>Forget key</button> : null}
        </div>
      </div>
      {editing ? (
        <form className="agent-byok-form" onSubmit={save}>
          <label htmlFor="nanocodex-openai-key">OpenAI API key</label>
          <input
            id="nanocodex-openai-key"
            ref={keyRef}
            type="password"
            autoComplete="new-password"
            placeholder="sk-…"
            disabled={busy}
            spellCheck={false}
          />
          <button type="submit" disabled={busy}>{busy ? "Starting…" : "Start one-hour session"}</button>
          <p>Your key is held server-side for one hour. This page receives only an HttpOnly session cookie.</p>
        </form>
      ) : null}
      {error ? <p className="agent-byok-error" role="alert">{error}</p> : null}
    </aside>
  );
}

async function credentialError(response: Response): Promise<string> {
  const payload = await response.json().catch(() => undefined) as { error?: unknown } | undefined;
  return typeof payload?.error === "string" ? payload.error : `Request failed with HTTP ${response.status}`;
}
