/**
 * Runs two follow-on prompts and closes every owned lifecycle handle.
 *
 * Long-lived applications normally retain the agent. This short-lived example
 * disposes it after reading the typed result of each completed Turn.
 */
export async function runOwnedSession(
  agent,
  {
    log = console.log,
    logDiagnostic = console.error,
  } = {},
) {
  const watch = agent.events.watch();
  const unwatch = watch.onEvent((event) => {
    if (event.type === "tool.call") {
      logDiagnostic(`tool: ${event.payload.tool}`);
    }
  });
  const turns = [];

  try {
    const first = agent.turn.prompt({
      input: "Use multiply to calculate 6 × 7. Return only the number.",
    });
    turns.push(first);
    const firstResult = await first.result();
    log("first:", firstResult.finalMessage);

    // Follow-on state, response IDs, and prompt-cache identity stay in Rust.
    const second = agent.turn.prompt({
      input: "Add one to that result. Return only the number.",
    });
    turns.push(second);
    const secondResult = await second.result();
    log("second:", secondResult.finalMessage);

    return { first: firstResult, second: secondResult };
  } finally {
    for (const turn of turns) turn.dispose();
    unwatch();
    watch.off();
    agent.dispose();
  }
}
