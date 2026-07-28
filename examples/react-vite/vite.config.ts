import { cloudflare } from "@cloudflare/vite-plugin";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const repositoryRoot = fileURLToPath(new URL("../..", import.meta.url));

export default defineConfig({
  plugins: [react(), cloudflare()],
  build: {
    manifest: true,
  },
  worker: { format: "es" },
  server: {
    fs: {
      // The example consumes the generated WASM package and browser host from
      // js/bindings without copying either artifact into the application.
      allow: [repositoryRoot],
    },
  },
});
