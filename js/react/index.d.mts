import type { ReactNode } from "react";

import type { Config, Snapshot, WorkerCommand, WorkerMessage } from "./config.mjs";

export {
  createConfig,
  type Config,
  type CreateConfigParameters,
  type Snapshot,
  type Status,
  type WorkerCommand,
  type WorkerLike,
  type WorkerMessage,
} from "./config.mjs";

export type Nanocodex<Command extends WorkerCommand = WorkerCommand> = Snapshot & {
  dispatch(command: Command): void;
  start(command?: Command): void;
  restart(command?: Command): void;
  disconnect(): void;
  stop(): void;
};

export function NanocodexProvider(props: {
  children: ReactNode;
  config: Config;
}): ReactNode;

export function useConfig<
  Command extends WorkerCommand = WorkerCommand,
  Message extends WorkerMessage = WorkerMessage,
>(): Config<Command, Message>;
export function useNanocodex<Command extends WorkerCommand = WorkerCommand>(): Nanocodex<Command>;
export function useNanocodexMessage<Message extends WorkerMessage = WorkerMessage>(
  listener: (message: Message) => void,
): void;
