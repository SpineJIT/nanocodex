import type { AgentEvent, PromptInput, TurnUsage } from "nanocodex";

export type ClientCommand =
  | { type: "prompt"; id: string; input: PromptInput }
  | { type: "steer"; id: string; input: PromptInput }
  | { type: "cancel"; id: string }
  | { type: "status" }
  | { type: "ping"; nonce?: string };

export type TurnCompleted = {
  type: "turn_completed";
  id: string;
  final_message: string;
  usage: TurnUsage;
};

export type ActiveTurn = {
  id: string;
  input: PromptInput;
};

export type ServerMessage =
  | { type: "ready"; session_id: string; restored: boolean; active_turns: string[]; active_turn_details: ActiveTurn[] }
  | { type: "turn_accepted"; id: string; input: PromptInput; replayed: boolean }
  | TurnCompleted
  | { type: "turn_failed"; id: string; error: string }
  | { type: "event"; event: AgentEvent }
  | { type: "status"; active_turns: string[]; active_turn_details: ActiveTurn[]; agent_loaded: boolean; connected_clients: number }
  | { type: "pong"; nonce?: string }
  | { type: "error"; code: string; message: string };

const TURN_ID = /^[A-Za-z0-9._:-]{1,128}$/;
const PROMPT_ITEM_TYPES = new Set(["text", "image", "audio"]);

export function parseCommand(encoded: string): ClientCommand {
  let value: unknown;
  try {
    value = JSON.parse(encoded);
  } catch {
    throw new ProtocolError("invalid_json", "messages must be JSON objects");
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new ProtocolError("invalid_message", "messages must be JSON objects");
  }
  const command = value as Record<string, unknown>;
  if (command.type === "ping") {
    if (command.nonce !== undefined && typeof command.nonce !== "string") {
      throw new ProtocolError("invalid_nonce", "ping nonce must be a string");
    }
    return { type: "ping", ...(command.nonce === undefined ? {} : { nonce: command.nonce }) };
  }
  if (command.type === "status") return { type: "status" };
  if (!["prompt", "steer", "cancel"].includes(String(command.type))) {
    throw new ProtocolError("unknown_command", "supported commands are prompt, steer, cancel, status, and ping");
  }
  if (typeof command.id !== "string" || !TURN_ID.test(command.id)) {
    throw new ProtocolError("invalid_turn_id", "turn id must be 1-128 safe ASCII characters");
  }
  const type = command.type as "prompt" | "steer" | "cancel";
  if (command.type === "cancel") return { type: "cancel", id: command.id };
  validateInput(command.input);
  return { type, id: command.id, input: command.input as PromptInput };
}

function validateInput(input: unknown): void {
  if (typeof input === "string") {
    if (!input.trim()) throw new ProtocolError("empty_prompt", "prompt input must not be empty");
    return;
  }
  if (!Array.isArray(input) || input.length === 0) {
    throw new ProtocolError("invalid_prompt", "prompt input must be text or a non-empty content array");
  }
  for (const item of input) {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new ProtocolError("invalid_prompt", "prompt content entries must be objects");
    }
    const type = (item as Record<string, unknown>).type;
    if (!PROMPT_ITEM_TYPES.has(String(type))) {
      throw new ProtocolError("invalid_prompt", "prompt content supports text, image, and audio entries");
    }
  }
}

export class ProtocolError extends Error {
  constructor(readonly code: string, message: string) {
    super(message);
  }
}
