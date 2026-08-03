import {
  createAgentOsActions,
  type AgentOsActorExtras,
  type AgentOsOptions,
} from "@rivet-dev/agentos";
import type { ToolMap } from "nanocodex";

const WORKSPACE = "/workspace";
const MAX_COMMAND_CHARS = 32 * 1024;
const MAX_FILE_BYTES = 1024 * 1024;
const MAX_OUTPUT_CHARS = 128 * 1024;
const MAX_LIST_ENTRIES = 512;
const MAX_TIMEOUT_MS = 120_000;

export const agentOsRuntimeOptions = {
  defaultSoftware: true,
  limits: {
    resources: {
      maxFilesystemBytes: 512 * 1024 * 1024,
      maxProcesses: 64,
      maxReaddirEntries: 4_096,
    },
  },
} satisfies AgentOsOptions;

export const agentOsPreviewOptions = {
  defaultExpiresInSeconds: 15 * 60,
  maxExpiresInSeconds: 60 * 60,
  maxActiveTokens: 32,
} satisfies NonNullable<AgentOsActorExtras["preview"]>;

const agentOsActions = createAgentOsActions(
  agentOsRuntimeOptions,
  undefined,
  agentOsPreviewOptions,
);

export type AgentOsActionContext = Parameters<typeof agentOsActions.exec>[0];

export function rivetSandboxTools(
  context: AgentOsActionContext,
  sessionId: string,
): ToolMap {
  return {
    sandbox_exec: {
      description: "Run a shell command in this actor's isolated persistent Rivet AgentOS workspace.",
      parameters: {
        type: "object",
        properties: {
          command: { type: "string", description: "Shell command to run." },
          cwd: { type: "string", description: "Workspace-relative working directory." },
          timeout_ms: { type: "integer", minimum: 1, maximum: MAX_TIMEOUT_MS },
        },
        required: ["command"],
        additionalProperties: false,
      },
      handler: async (input) => {
        const value = objectInput(input);
        const command = requiredString(value.command, "command", MAX_COMMAND_CHARS);
        const cwd = workspacePath(optionalString(value.cwd, "cwd") ?? ".");
        const timeout = optionalInteger(value.timeout_ms, "timeout_ms", 1, MAX_TIMEOUT_MS) ?? 60_000;
        const result = await agentOsActions.exec(context, command, {
          cwd,
          timeout,
          captureStdio: true,
        });
        const stdout = truncate(result.stdout);
        const stderr = truncate(result.stderr);
        return {
          success: result.exitCode === 0,
          exit_code: result.exitCode,
          stdout: stdout.text,
          stderr: stderr.text,
          stdout_truncated: stdout.truncated,
          stderr_truncated: stderr.truncated,
        };
      },
    },
    sandbox_read_file: {
      description: "Read a UTF-8 text file from this actor's isolated workspace (maximum 1 MiB).",
      parameters: pathParameters(),
      handler: async (input) => {
        const path = workspacePath(requiredString(objectInput(input).path, "path", 1024));
        const content = await agentOsActions.readFile(context, path);
        if (content.byteLength > MAX_FILE_BYTES) throw new Error("file exceeds 1 MiB");
        return { path, content: new TextDecoder().decode(content) };
      },
    },
    sandbox_write_file: {
      description: "Write a UTF-8 text file inside this actor's isolated workspace (maximum 1 MiB).",
      parameters: {
        type: "object",
        properties: {
          path: { type: "string", description: "Workspace-relative file path." },
          content: { type: "string", description: "Complete UTF-8 file content." },
        },
        required: ["path", "content"],
        additionalProperties: false,
      },
      handler: async (input) => {
        const value = objectInput(input);
        const path = workspacePath(requiredString(value.path, "path", 1024));
        const content = requiredString(value.content, "content", MAX_FILE_BYTES);
        const bytes = Buffer.byteLength(content, "utf8");
        if (bytes > MAX_FILE_BYTES) throw new Error("content exceeds 1 MiB");
        const parent = path.slice(0, path.lastIndexOf("/")) || WORKSPACE;
        await agentOsActions.mkdir(context, parent, { recursive: true });
        await agentOsActions.writeFile(context, path, content);
        return { path, bytes_written: bytes };
      },
    },
    sandbox_list_files: {
      description: "List files in a directory inside this actor's isolated workspace.",
      parameters: {
        type: "object",
        properties: {
          path: { type: "string", description: "Workspace-relative directory; defaults to the workspace root." },
        },
        additionalProperties: false,
      },
      handler: async (input) => {
        const path = workspacePath(optionalString(objectInput(input).path, "path") ?? ".");
        const entries = await agentOsActions.readdirEntries(context, path);
        return {
          path,
          entries: entries.slice(0, MAX_LIST_ENTRIES).map((entry) => ({
            name: entry.name,
            type: entry.isDirectory ? "directory" : entry.isSymbolicLink ? "symlink" : "file",
          })),
          truncated: entries.length > MAX_LIST_ENTRIES,
        };
      },
    },
    sandbox_preview: {
      description: "Expose a server running in AgentOS through a temporary Rivet Actor preview URL.",
      parameters: {
        type: "object",
        properties: {
          port: { type: "integer", minimum: 1024, maximum: 65_535 },
          ttl_seconds: { type: "integer", minimum: 60, maximum: 3_600 },
        },
        required: ["port"],
        additionalProperties: false,
      },
      handler: async (input) => {
        const value = objectInput(input);
        const port = requiredInteger(value.port, "port", 1024, 65_535);
        const ttl = optionalInteger(value.ttl_seconds, "ttl_seconds", 60, 3_600) ?? 15 * 60;
        const preview = await agentOsActions.createPreviewUrl(context, port, ttl);
        const publicUrl = process.env.NANOCODEX_PUBLIC_URL?.replace(/\/$/, "");
        return {
          port,
          path: preview.path,
          url: publicUrl ? `${publicUrl}${preview.path}` : preview.path,
          expires_at: new Date(preview.expiresAt).toISOString(),
          persistent: false,
        };
      },
    },
  };
}

export function workspacePath(raw: string): string {
  if (!raw || raw.length > 1024 || raw.includes("\0")) throw new Error("path must be 1-1024 characters");
  let relative = raw;
  if (relative === WORKSPACE) relative = ".";
  else if (relative.startsWith(`${WORKSPACE}/`)) relative = relative.slice(WORKSPACE.length + 1);
  else if (relative.startsWith("/")) throw new Error("path must be relative to /workspace");
  const parts = relative.split("/").filter((part) => part !== "" && part !== ".");
  if (parts.includes("..")) throw new Error("path must not contain '..'");
  return parts.length === 0 ? WORKSPACE : `${WORKSPACE}/${parts.join("/")}`;
}

function objectInput(input: unknown): Record<string, unknown> {
  if (!input || typeof input !== "object" || Array.isArray(input)) throw new Error("tool input must be an object");
  return input as Record<string, unknown>;
}

function requiredString(value: unknown, name: string, maxChars: number): string {
  if (typeof value !== "string" || value.length === 0) throw new Error(`${name} must be a non-empty string`);
  if (value.length > maxChars) throw new Error(`${name} is too long`);
  return value;
}

function optionalString(value: unknown, name: string): string | undefined {
  if (value === undefined) return undefined;
  return requiredString(value, name, 1024);
}

function requiredInteger(value: unknown, name: string, minimum: number, maximum: number): number {
  const parsed = optionalInteger(value, name, minimum, maximum);
  if (parsed === undefined) throw new Error(`${name} is required`);
  return parsed;
}

function optionalInteger(
  value: unknown,
  name: string,
  minimum: number,
  maximum: number,
): number | undefined {
  if (value === undefined) return undefined;
  if (!Number.isInteger(value) || (value as number) < minimum || (value as number) > maximum) {
    throw new Error(`${name} must be an integer between ${minimum} and ${maximum}`);
  }
  return value as number;
}

function pathParameters(): Record<string, unknown> {
  return {
    type: "object",
    properties: { path: { type: "string", description: "Workspace-relative file path." } },
    required: ["path"],
    additionalProperties: false,
  };
}

function truncate(value: string): { text: string; truncated: boolean } {
  return value.length <= MAX_OUTPUT_CHARS
    ? { text: value, truncated: false }
    : { text: value.slice(0, MAX_OUTPUT_CHARS), truncated: true };
}
