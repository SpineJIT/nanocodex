import { setup } from "rivetkit";

import { nanocodexAuth } from "./auth.js";
import { nanocodex } from "./actors.js";

export const registry = setup({
  // Leave room for the JSON/RPC envelope so the actor's 1 MiB prompt limit is
  // the authoritative boundary instead of RivetKit's 64 KiB transport default.
  maxIncomingMessageSize: 2 * 1024 * 1024,
  // The embedding server owns its HTTP listener and process lifecycle, so it
  // coordinates both listeners and the child engine under one signal handler.
  shutdown: {
    disableSignalHandlers: true,
    gracePeriodMs: 15_000,
  },
  use: {
    nanocodex,
    nanocodexAuth,
  },
});
