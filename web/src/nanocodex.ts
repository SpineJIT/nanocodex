import { createConfig } from "nanocodex-react";
import type { TuiCommand, TuiMessage } from "nanocodex-tui";

export type AgentTransport = "openai" | "mpp";
export type WebTuiCommand =
  | Exclude<TuiCommand, { type: "start" }>
  | (Extract<TuiCommand, { type: "start" }> & { transport: AgentTransport });
export type PaymentStatus = {
  rootAddress: string;
  accessKeyAddress: string;
  channelId?: string;
  cumulative: string;
};
export type WebTuiMessage = TuiMessage
  | { type: "mppPayment"; payment: PaymentStatus }
  | { type: "mppJsonl"; line: string };

/** Website-owned wiring for the publishable React package. */
export const nanocodexConfig = createConfig<WebTuiCommand, WebTuiMessage>({
  autoStart: false,
  worker: () => new Worker(new URL("./agent.worker.ts", import.meta.url), { type: "module" }),
});
