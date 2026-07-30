import { setup } from "rivetkit";

import { nanocodexAuth } from "./auth.js";
import { nanocodex } from "./actors.js";

export const registry = setup({
  // Leave room for the JSON/RPC envelope so the actor's 1 MiB prompt limit is
  // the authoritative boundary instead of RivetKit's 64 KiB transport default.
  maxIncomingMessageSize: 2 * 1024 * 1024,
  use: {
    nanocodex,
    nanocodexAuth,
  },
});
