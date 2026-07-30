import { DurableObject } from "cloudflare:workers";

const OAUTH_CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_TOKEN_ENDPOINT = "https://auth.openai.com/oauth/token";
const REFRESH_EARLY_MS = 5 * 60_000;
const MAX_TOKEN_RESPONSE_BYTES = 16 * 1024;

export interface SubscriptionAuthEnv {
  CHATGPT_ACCESS_TOKEN?: string;
  CHATGPT_ACCOUNT_ID?: string;
  CHATGPT_FEDRAMP?: string;
  CHATGPT_REFRESH_TOKEN?: string;
  CHATGPT_TOKEN_ENDPOINT?: string;
}

export type SubscriptionSnapshot = {
  bearerToken: string;
  accountId: string;
  fedramp: boolean;
  revision: number;
};

type CredentialRow = {
  access_token: string;
  refresh_token: string;
  account_id: string;
  fedramp: number;
  revision: number;
  expires_at: number | null;
  refreshed_at: number;
};

type RefreshResponse = {
  access_token?: unknown;
  refresh_token?: unknown;
  id_token?: unknown;
};

export class NanocodexSubscriptionAuth extends DurableObject<SubscriptionAuthEnv> {
  #refreshing?: { revision: number; promise: Promise<CredentialRow> };

  constructor(ctx: DurableObjectState, env: SubscriptionAuthEnv) {
    super(ctx, env);
    this.ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS credentials (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        access_token TEXT NOT NULL,
        refresh_token TEXT NOT NULL,
        account_id TEXT NOT NULL,
        fedramp INTEGER NOT NULL,
        revision INTEGER NOT NULL,
        expires_at INTEGER,
        refreshed_at INTEGER NOT NULL
      );
    `);
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    try {
      if (request.method === "POST" && url.pathname === "/snapshot") {
        let current = this.#credential();
        if (!current) current = this.#seed();
        if (current.expires_at !== null && current.expires_at <= Date.now() + REFRESH_EARLY_MS) {
          current = await this.#refresh(current.revision);
        }
        return Response.json(toSnapshot(current), { headers: { "cache-control": "no-store" } });
      }
      if (request.method === "POST" && url.pathname === "/recover") {
        const body = await request.json<{ revision?: unknown }>();
        if (!Number.isSafeInteger(body.revision) || Number(body.revision) < 0) {
          return Response.json({ error: "invalid revision" }, { status: 400 });
        }
        let current = this.#credential();
        if (!current) current = this.#seed();
        current = await this.#refresh(Number(body.revision));
        return Response.json(toSnapshot(current), { headers: { "cache-control": "no-store" } });
      }
      if (request.method === "GET" && url.pathname === "/status") {
        const current = this.#credential();
        return Response.json({
          configured: current !== undefined,
          account_id: current?.account_id,
          revision: current?.revision,
          expires_at: current?.expires_at,
          refreshed_at: current?.refreshed_at,
        }, { headers: { "cache-control": "no-store" } });
      }
      if (request.method === "DELETE" && url.pathname === "/credentials") {
        if (this.#refreshing) {
          try { await this.#refreshing.promise; } catch { /* Reset still wins after a failed refresh. */ }
        }
        this.ctx.storage.sql.exec("DELETE FROM credentials");
        return new Response(null, { status: 204 });
      }
      return Response.json({ error: "not_found" }, { status: 404 });
    } catch (error) {
      return Response.json({ error: safeError(error) }, {
        status: 503,
        headers: { "cache-control": "no-store" },
      });
    }
  }

  #seed(): CredentialRow {
    const accessToken = requiredSecret(this.env.CHATGPT_ACCESS_TOKEN, "CHATGPT_ACCESS_TOKEN");
    const refreshToken = requiredSecret(this.env.CHATGPT_REFRESH_TOKEN, "CHATGPT_REFRESH_TOKEN");
    const accountId = requiredSecret(this.env.CHATGPT_ACCOUNT_ID, "CHATGPT_ACCOUNT_ID");
    const now = Date.now();
    const expiresAt = jwtExpiration(accessToken);
    const fedramp = this.env.CHATGPT_FEDRAMP === "true";
    this.ctx.storage.sql.exec(
      `INSERT INTO credentials
       (singleton, access_token, refresh_token, account_id, fedramp, revision, expires_at, refreshed_at)
       VALUES (1, ?, ?, ?, ?, 0, ?, ?)`,
      accessToken,
      refreshToken,
      accountId,
      fedramp ? 1 : 0,
      expiresAt,
      now,
    );
    return {
      access_token: accessToken,
      refresh_token: refreshToken,
      account_id: accountId,
      fedramp: fedramp ? 1 : 0,
      revision: 0,
      expires_at: expiresAt,
      refreshed_at: now,
    };
  }

  async #refresh(rejectedRevision: number): Promise<CredentialRow> {
    const current = this.#credential();
    if (!current) throw new Error("ChatGPT credentials are not initialized");
    if (current.revision !== rejectedRevision) return current;

    if (this.#refreshing?.revision === rejectedRevision) return this.#refreshing.promise;
    const promise = this.#performRefresh(current);
    this.#refreshing = { revision: rejectedRevision, promise };
    try {
      return await promise;
    } finally {
      if (this.#refreshing?.promise === promise) this.#refreshing = undefined;
    }
  }

  async #performRefresh(current: CredentialRow): Promise<CredentialRow> {
    const response = await fetch(this.env.CHATGPT_TOKEN_ENDPOINT ?? DEFAULT_TOKEN_ENDPOINT, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        client_id: OAUTH_CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token: current.refresh_token,
      }),
    });
    const encoded = await readBoundedText(response, MAX_TOKEN_RESPONSE_BYTES);
    if (!response.ok) {
      const code = parseRefreshErrorCode(encoded);
      throw new Error(code
        ? `ChatGPT token refresh was rejected: ${code}`
        : `ChatGPT token refresh failed with HTTP ${response.status}`);
    }
    let refreshed: RefreshResponse;
    try {
      refreshed = JSON.parse(encoded) as RefreshResponse;
    } catch {
      throw new Error("ChatGPT token refresh returned invalid JSON");
    }
    const accessToken = optionalString(refreshed.access_token);
    if (accessToken === undefined) throw new Error("ChatGPT token refresh omitted access_token");
    const refreshToken = optionalString(refreshed.refresh_token) ?? current.refresh_token;
    const idToken = optionalString(refreshed.id_token);
    const claims = idToken === undefined ? undefined : jwtPayload(idToken);
    const claimedAccount = nestedString(claims, "https://api.openai.com/auth", "chatgpt_account_id");
    if (claimedAccount !== undefined && claimedAccount !== current.account_id) {
      throw new Error("the refreshed ChatGPT credential changed accounts");
    }
    const claimedFedramp = nestedBoolean(
      claims,
      "https://api.openai.com/auth",
      "chatgpt_account_is_fedramp",
    );
    const next: CredentialRow = {
      access_token: accessToken,
      refresh_token: refreshToken,
      account_id: current.account_id,
      fedramp: claimedFedramp === undefined ? current.fedramp : (claimedFedramp ? 1 : 0),
      revision: current.revision + 1,
      expires_at: jwtExpiration(accessToken),
      refreshed_at: Date.now(),
    };
    this.ctx.storage.sql.exec(
      `UPDATE credentials SET
         access_token = ?, refresh_token = ?, fedramp = ?, revision = ?, expires_at = ?, refreshed_at = ?
       WHERE singleton = 1 AND revision = ?`,
      next.access_token,
      next.refresh_token,
      next.fedramp,
      next.revision,
      next.expires_at,
      next.refreshed_at,
      current.revision,
    );
    return next;
  }

  #credential(): CredentialRow | undefined {
    return this.ctx.storage.sql.exec<CredentialRow>(
      `SELECT access_token, refresh_token, account_id, fedramp, revision, expires_at, refreshed_at
       FROM credentials WHERE singleton = 1`,
    ).toArray()[0];
  }
}

function toSnapshot(row: CredentialRow): SubscriptionSnapshot {
  return {
    bearerToken: row.access_token,
    accountId: row.account_id,
    fedramp: row.fedramp !== 0,
    revision: row.revision,
  };
}

function requiredSecret(value: string | undefined, name: string): string {
  if (!value?.trim()) throw new Error(`${name} is not configured`);
  return value;
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value : undefined;
}

function jwtExpiration(token: string): number | null {
  const payload = jwtPayload(token);
  return typeof payload?.exp === "number" && Number.isFinite(payload.exp)
    ? payload.exp * 1000
    : null;
}

function jwtPayload(token: string): Record<string, unknown> | undefined {
  const encoded = token.split(".")[1];
  if (!encoded) return undefined;
  try {
    const base64 = encoded.replaceAll("-", "+").replaceAll("_", "/").padEnd(
      encoded.length + ((4 - encoded.length % 4) % 4),
      "=",
    );
    return JSON.parse(atob(base64)) as Record<string, unknown>;
  } catch {
    return undefined;
  }
}

function nestedString(value: Record<string, unknown> | undefined, objectKey: string, key: string) {
  const nested = value?.[objectKey];
  if (!nested || typeof nested !== "object" || Array.isArray(nested)) return undefined;
  const found = (nested as Record<string, unknown>)[key];
  return typeof found === "string" ? found : undefined;
}

function nestedBoolean(value: Record<string, unknown> | undefined, objectKey: string, key: string) {
  const nested = value?.[objectKey];
  if (!nested || typeof nested !== "object" || Array.isArray(nested)) return undefined;
  const found = (nested as Record<string, unknown>)[key];
  return typeof found === "boolean" ? found : undefined;
}

async function readBoundedText(response: Response, limit: number): Promise<string> {
  if (!response.body) return "";
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let total = 0;
  let body = "";
  while (true) {
    const { done, value } = await reader.read();
    if (done) return body + decoder.decode();
    total += value.byteLength;
    if (total > limit) {
      await reader.cancel();
      throw new Error(`ChatGPT token response exceeded ${limit} bytes`);
    }
    body += decoder.decode(value, { stream: true });
  }
}

function parseRefreshErrorCode(body: string): string | undefined {
  try {
    const parsed = JSON.parse(body) as { error?: unknown };
    if (typeof parsed.error === "string") return parsed.error;
    if (parsed.error && typeof parsed.error === "object" && !Array.isArray(parsed.error)) {
      return optionalString((parsed.error as Record<string, unknown>).code);
    }
  } catch { /* The status remains sufficient when the body is not JSON. */ }
  return undefined;
}

function safeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
