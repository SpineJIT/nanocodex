import type {
  AgentOptions,
  DefaultAgent,
  MppSession,
  ToolMap,
} from "../types.mjs";

export type Agent = DefaultAgent;

/** Creates a Node-hosted Rust/WASM Agent. */
export function create(options: create.Options): Promise<create.ReturnType>;
export declare namespace create {
  type Options = AgentOptions & ({ apiKey: string; mpp?: never } | { apiKey?: never; mpp: MppSession }) & {
    apiBaseUrl?: string | undefined;
    module?: unknown;
    tools?: ToolMap | undefined;
    websocketUrl?: string | undefined;
  };
  type ReturnType = Agent;
}
