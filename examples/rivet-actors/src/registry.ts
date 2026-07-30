import { setup } from "@rivet-dev/agentos";

import { nanocodexAuth } from "./auth.js";
import { nanocodex, nanocodexWorkspace } from "./actors.js";

export const registry = setup({
  use: {
    nanocodex,
    nanocodexAuth,
    nanocodexWorkspace,
  },
});
