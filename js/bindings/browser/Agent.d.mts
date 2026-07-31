import type {
  AgentOptions,
  DefaultAgent,
  MppSession,
  ToolMap,
} from "../types.mjs";
import type {
  BrowserWebSocketConnection,
  BrowserWebSocketRequest,
} from "./host.mjs";

export type Agent = DefaultAgent;

/** Creates a browser- or Worker-hosted Rust/WASM Agent. */
export function create(options?: create.Options): Promise<create.ReturnType>;
export declare namespace create {
  type Options = AgentOptions & (
    | { apiKey?: string | undefined; hostAuth?: never; mpp?: never }
    | { apiKey?: never; hostAuth?: true; mpp?: never }
    | { apiKey?: never; hostAuth?: never; mpp: MppSession }
  ) & {
    WebSocketImpl?: typeof WebSocket | undefined;
    apiBaseUrl?: string | undefined;
    createWebSocket?(
      endpoint: string,
      sessionId: string,
      request: BrowserWebSocketRequest,
    ): WebSocket | BrowserWebSocketConnection | Promise<WebSocket | BrowserWebSocketConnection>;
    module?: unknown;
    tools?: ToolMap | undefined;
    /** Direct dispatch is CSP-safe; Code Mode requires dynamic JavaScript evaluation. */
    toolMode?: "code" | "direct" | undefined;
    websocketUrl?: string | undefined;
  };
  type ReturnType = Agent;
}
