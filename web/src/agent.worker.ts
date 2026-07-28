import { Agent } from "nanocodex/browser";
import {
  createAgentController,
  type AgentControllerStart,
  type AgentControllerTools,
} from "./agentController";
import { createBrowserTools } from "./browserTools";
import type { WebTuiCommand } from "./nanocodex";
import { createPaymentSessionOwner } from "./paymentSessionOwner";

type IncomingMessage = WebTuiCommand;
type PaymentSession = Awaited<ReturnType<(typeof import("./tempo"))["createTempoMppSession"]>>;

type WorkerScope = {
  location: Location;
  onmessage: ((event: MessageEvent<IncomingMessage>) => void) | null;
  postMessage(message: unknown): void;
};

const worker = self as unknown as WorkerScope;
const paymentSessions = createPaymentSessionOwner<PaymentSession>();
const controller = createAgentController({
  createAgent,
  postMessage: (message) => worker.postMessage(message),
  logPaymentEvent: (event) => console.info(JSON.stringify(event)),
});
let commands = Promise.resolve();

worker.onmessage = ({ data }: MessageEvent<IncomingMessage>) => {
  commands = commands
    .then(() => controller.handle(data))
    .catch((error) => {
      worker.postMessage({
        type: "fatal",
        message: errorMessage(error),
      });
    });
};

async function createAgent(
  start: AgentControllerStart,
  tools: AgentControllerTools,
) {
  await paymentSessions.clear();
  const common = {
    tools: {
      ...createBrowserTools({
        recentImages: tools.recentImages,
        rememberImage: tools.rememberImage,
      }),
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
    thinking: start.thinking,
    reasoningMode: start.reasoningMode,
  };
  if (start.transport === "mpp") {
    const paymentKey = start.paymentKey;
    if (!paymentKey) {
      throw new Error("MPP requires an authorized Tempo access key");
    }
    const { createTempoMppSession } = await import("./tempo");
    return paymentSessions.open(
      () => createTempoMppSession(paymentKey),
      async (paymentSession) => {
        const agent = await Agent.create({
          ...common,
          fastMode: true,
          mpp: paymentSession.mpp,
        });
        return {
          agent,
          payment: {
            rootAddress: paymentSession.rootAddress,
            accessKeyAddress: paymentSession.accessKeyAddress,
            get channelId() {
              return paymentSession.mpp.channelId;
            },
            cumulative: () => paymentSession.mpp.cumulative.toString(),
          },
        };
      },
    );
  }
  return {
    agent: await Agent.create({
      ...common,
      apiKey: "worker-managed",
      websocketUrl: workerEndpoint(),
      createWebSocket: (endpoint: string, sessionId: string) => {
        const url = new URL(endpoint);
        url.searchParams.set("session_id", sessionId);
        return new WebSocket(url);
      },
    }),
  };
}

function workerEndpoint(): string {
  const protocol = self.location.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${self.location.host}/api/responses`;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
