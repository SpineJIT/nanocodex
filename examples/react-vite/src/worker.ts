import { Agent, type ReasoningMode, type Thinking } from "nanocodex/browser";

type StartMessage = {
  type: "start";
  transport: "openai" | "mpp";
  thinking: Thinking;
  reasoningMode?: ReasoningMode;
};

type PromptMessage = {
  type: "prompt";
  id: number;
  prompt: string;
};

type IncomingMessage = StartMessage | PromptMessage;
type PaymentSession = Awaited<
  ReturnType<(typeof import("./tempo"))["createTempoMppSession"]>
>;

const worker = self as DedicatedWorkerGlobalScope;

let agent: Agent.Agent | undefined;
let eventWatch: ReturnType<Agent.Agent["events"]["watch"]> | undefined;
let paymentSession: PaymentSession | undefined;

worker.onmessage = ({ data }: MessageEvent<IncomingMessage>) => {
  void handleMessage(data);
};

async function handleMessage(data: IncomingMessage): Promise<void> {
  if (data.type === "start") {
    eventWatch?.off();
    eventWatch = undefined;
    agent?.dispose();
    paymentSession = undefined;
    const common = {
      tools: {
        browserInfo: {
          description: "Return basic information about the browser Worker runtime.",
          parameters: { type: "object", additionalProperties: false },
          handler: async () => ({
            language: navigator.language,
            online: navigator.onLine,
            userAgent: navigator.userAgent,
          }),
        },
      },
      thinking: data.thinking,
      reasoningMode: data.reasoningMode,
    };
    if (data.transport === "mpp") {
      const { createTempoMppSession } = await import("./tempo");
      const created = await createTempoMppSession();
      paymentSession = created;
      agent = await Agent.create({ ...common, mpp: created.mpp });
    } else {
      agent = await Agent.create({
        ...common,
        apiKey: "worker-managed",
        websocketUrl: workerEndpoint(),
        createWebSocket: (endpoint: string, sessionId: string) => {
          const url = new URL(endpoint);
          url.searchParams.set("session_id", sessionId);
          return new WebSocket(url);
        },
      });
    }
    eventWatch = agent.events.watch();
    eventWatch.onEvent((event) => worker.postMessage({ type: "event", event }));
    worker.postMessage({
      type: "ready",
      transport: data.transport,
      ...(paymentSession
        ? {
            rootAddress: paymentSession.rootAddress,
            accessKeyAddress: paymentSession.accessKeyAddress,
            channelId: paymentSession.mpp.channelId,
          }
        : {}),
    });
    return;
  }

  const current = agent;
  if (!current) {
    worker.postMessage({ type: "error", id: data.id, message: "Start the agent first." });
    return;
  }

  // Each prompt gets an independent Turn, while the owned agent serializes
  // them onto the same session and preserves all follow-on context.
  const turn = current.turn.prompt({ input: data.prompt });
  void turn.result().then(
    (message) => worker.postMessage({
      type: "result",
      id: data.id,
      message,
      payment: paymentSession
        ? {
            channelId: paymentSession.mpp.channelId,
            cumulative: paymentSession.mpp.cumulative.toString(),
          }
        : undefined,
    }),
    (error) => worker.postMessage({
      type: "error",
      id: data.id,
      message: error instanceof Error ? error.message : String(error),
    }),
  );
}

function workerEndpoint(): string {
  const protocol = self.location.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${self.location.host}/api/responses`;
}
