import type {
  AgentEvent,
  DefaultAgent,
  Turn,
} from "nanocodex";

import type {
  AgentWorkerCommand,
  AgentWorkerMessage,
  StartMessage,
} from "./protocol";

export type ExamplePayment = {
  rootAddress: string;
  accessKeyAddress: string;
  channelId?: string;
  cumulative(): string;
};

export type ExampleAgentControllerDependencies = {
  createAgent(start: StartMessage): Promise<{
    agent: DefaultAgent;
    payment?: ExamplePayment;
  }>;
  postMessage(message: AgentWorkerMessage): void;
};

/** Thin lifecycle owner for the React example's dedicated Worker. */
export function createExampleAgentController({
  createAgent,
  postMessage,
}: ExampleAgentControllerDependencies) {
  let agent: DefaultAgent | undefined;
  let eventWatch: ReturnType<DefaultAgent["events"]["watch"]> | undefined;
  let payment: ExamplePayment | undefined;
  let generation = 0;
  let disposed = false;
  const turns = new Set<Turn>();
  const releasedTurns = new WeakSet<Turn>();

  async function handle(command: AgentWorkerCommand): Promise<void> {
    if (disposed) throw new Error("Agent controller is disposed");
    if (command.type === "start") {
      reset();
      const currentGeneration = generation;
      const created = await createAgent(command);
      if (disposed || currentGeneration !== generation) {
        created.agent.dispose();
        return;
      }
      agent = created.agent;
      payment = created.payment;
      eventWatch = agent.events.watch();
      const watchedAgent = agent;
      eventWatch.onEvent((event) => {
        if (
          disposed
          || currentGeneration !== generation
          || agent !== watchedAgent
        ) {
          return;
        }
        postMessage({ type: "event", event });
      });
      postMessage({
        type: "ready",
        transport: command.transport,
        ...(payment
          ? {
              rootAddress: payment.rootAddress,
              accessKeyAddress: payment.accessKeyAddress,
              channelId: payment.channelId,
            }
          : {}),
      });
      return;
    }

    const current = agent;
    if (!current) {
      postMessage({
        type: "error",
        id: command.id,
        message: "Start the agent first.",
      });
      return;
    }

    let turn: Turn;
    try {
      turn = current.turn.prompt({ input: command.prompt });
    } catch (error) {
      postMessage({
        type: "error",
        id: command.id,
        message: errorMessage(error),
      });
      return;
    }
    turns.add(turn);
    const turnGeneration = generation;
    void Promise.resolve()
      .then(() => turn.result())
      .then(
        (result) => {
          if (disposed || turnGeneration !== generation) return;
          postMessage({
            type: "result",
            id: command.id,
            message: result.finalMessage,
            payment: payment
              ? {
                  channelId: payment.channelId,
                  cumulative: payment.cumulative(),
                }
              : undefined,
          });
        },
        (error) => {
          if (disposed || turnGeneration !== generation) return;
          postMessage({
            type: "error",
            id: command.id,
            message: errorMessage(error),
          });
        },
      )
      .finally(() => {
        turns.delete(turn);
        releaseTurn(turn);
      });
  }

  function reset(): void {
    generation += 1;
    eventWatch?.off();
    eventWatch = undefined;
    for (const turn of turns) releaseTurn(turn);
    turns.clear();
    agent?.dispose();
    agent = undefined;
    payment = undefined;
  }

  function releaseTurn(turn: Turn): void {
    if (releasedTurns.has(turn)) return;
    releasedTurns.add(turn);
    turn.dispose();
  }

  function dispose(): void {
    if (disposed) return;
    disposed = true;
    reset();
  }

  return Object.freeze({ handle, dispose });
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
