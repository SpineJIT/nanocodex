export type Thinking = "none" | "low" | "medium" | "high" | "xhigh" | "max";
export type ReasoningMode = "standard" | "pro";

export type PromptItem =
  | { type: "text"; text: string }
  | { type: "image"; image_url: string; detail?: "auto" | "low" | "high" | "original" | undefined }
  | { type: "audio"; audio_url: string };

export type PromptInput = string | readonly PromptItem[];

export type AgentEvent = {
  protocol_version: number;
  request_id: string;
  seq: number;
  type: string;
  payload: Record<string, unknown>;
};

export type AgentOptions = {
  instructions?: string | undefined;
  pricing?: PricingSnapshot | undefined;
  reasoningMode?: ReasoningMode | undefined;
  fastMode?: boolean | undefined;
  sessionId?: string | undefined;
  thinking?: Thinking | undefined;
  resume?: SessionSnapshot | undefined;
};

export type PricingSnapshot = Readonly<{
  id: string;
  source: string;
  effective_date: string;
  model: "gpt-5.6-sol";
  rates: Readonly<{
    input_usd_per_million: string;
    cached_input_usd_per_million: string;
    cache_write_input_usd_per_million: string;
    output_usd_per_million: string;
  }>;
}>;

export type EstimatedUsdCost = Readonly<{
  usd: string;
  input_usd: string;
  cached_input_usd: string;
  cache_write_input_usd: string;
  output_usd: string;
  pricing: PricingSnapshot;
}>;

export type CostStatus =
  | "estimated_from_usage"
  | "pricing_not_configured"
  | "usage_not_reported"
  | "other";

export type SessionSnapshot = Readonly<{
  version: number;
  model: string;
  lineage_id: string;
  prompt_cache_key: string;
  workspace: string;
  request_prefix?: readonly Record<string, unknown>[] | undefined;
  canonical_context: Record<string, unknown>;
  history: readonly Record<string, unknown>[];
}>;

export type TurnUsage = Readonly<{
  input_tokens: number;
  cached_input_tokens: number;
  cache_write_input_tokens: number;
  output_tokens: number;
  reasoning_output_tokens: number;
  total_tokens: number;
  estimated_cost: EstimatedUsdCost | null;
  cost_status: CostStatus;
}>;

export type ForkOptions = { at?: Turn | undefined };
export type WatchEventsOptions = { includeAllSessions?: boolean | undefined };

export type EventWatcher = Readonly<{
  onEvent(listener: (event: AgentEvent) => void): () => void;
  off(): void;
  [Symbol.asyncIterator](): AsyncIterableIterator<AgentEvent>;
}>;

export type AgentActions = {
  events: {
    watch(options?: WatchEventsOptions): EventWatcher;
  };
  session: {
    fork(options?: ForkOptions): Promise<DefaultAgent>;
    setFastMode(enabled: boolean): Promise<void>;
    setThinking(thinking: Thinking): Promise<void>;
    spawn(): Promise<DefaultAgent>;
  };
  turn: {
    prompt(options: { input: PromptInput }): Turn;
  };
};

export type Agent<extended extends object = {}> = {
  readonly key: string;
  readonly name: string;
  readonly sessionId: string;
  readonly type: string;
  readonly uid: string;
  extend<const extension extends object>(
    decorator: (agent: Agent<extended>) => extension,
  ): Agent<extended & extension>;
  dispose(): void;
} & extended;

export type DefaultAgent = Agent<AgentActions>;

export type Turn<agent extends Agent<object> = Agent<object>> = Readonly<{
  readonly agent: agent;
  result(): Promise<string>;
  snapshot(): SessionSnapshot;
  usage(): TurnUsage;
  steer(options: { input: PromptInput }): Promise<void>;
  cancel(): Promise<void>;
  dispose(): void;
}>;

export type ToolContext = {
  callId: string;
  parentCallId: string;
  sessionId: string;
};

export type Tool = {
  description: string;
  parameters: Record<string, unknown>;
  handler(input: unknown, context: ToolContext): unknown | Promise<unknown>;
};

export type ToolMap = Record<string, Tool>;

/** A paid WebSocket session, such as an mppx Tempo session manager. */
export type MppSession = {
  ws(endpoint: string | URL): Promise<MppWebSocket>;
  close?(): unknown | Promise<unknown>;
};

export type MppWebSocket = {
  readonly readyState: number;
  readonly bufferedAmount?: number | undefined;
  addEventListener(type: string, listener: (event: any) => void, options?: unknown): void;
  send(message: string): void;
  close(code?: number, reason?: string): void;
};
