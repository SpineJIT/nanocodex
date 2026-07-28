import type {
  AgentOptions,
  DefaultAgent,
  MppSession,
  ToolMap,
} from "../types.mjs";

export type Agent = DefaultAgent;

/** Creates a browser- or Worker-hosted Rust/WASM Agent. */
export function create(options?: create.Options): Promise<create.ReturnType>;
export declare namespace create {
  type Options = AgentOptions & (
    | { apiKey?: string | undefined; mpp?: never }
    | { apiKey?: never; mpp: MppSession }
  ) & {
    WebSocketImpl?: typeof WebSocket | undefined;
    apiBaseUrl?: string | undefined;
    createWebSocket?(endpoint: string, sessionId: string): WebSocket;
    module?: unknown;
    tools?: ToolMap | undefined;
    websocketUrl?: string | undefined;
  };
  type ReturnType = Agent;
}
