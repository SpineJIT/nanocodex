import { createClient } from "@rivet-dev/agentos/client";

import type { registry } from "./registry.js";

export function createNanocodexClient(endpoint = "http://127.0.0.1:6420") {
  return createClient<typeof registry>({ endpoint });
}
