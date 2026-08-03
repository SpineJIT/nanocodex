import { cloudflare } from "@cloudflare/vite-plugin";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import mkcert from "vite-plugin-mkcert";

const repositoryRoot = fileURLToPath(new URL("..", import.meta.url));

export default defineConfig({
  // Tempo Wallet embeds in an iframe only on HTTPS. A trusted local
  // certificate keeps the development flow identical to production and lets
  // the hosted wallet perform cross-origin passkey ceremonies in the embed.
  plugins: [mkcert(), react(), cloudflare()],
  build: {
    // The production graph gate consumes this manifest so it measures complete
    // static import closures instead of whichever output chunk happens to keep
    // the entry-point name.
    manifest: true,
  },
  resolve: {
    preserveSymlinks: true,
    dedupe: [
      "react",
      "react-dom",
      "nanocodex-react",
      "nanocodex-tui",
      "@pierre/theme",
      "@shikijs/core",
      "@shikijs/engine-javascript",
      "@shikijs/langs",
      "@shikijs/primitive",
      "@shikijs/types",
      "@tanstack/react-virtual",
      "shiki",
      "streamdown",
    ],
  },
  worker: { format: "es" },
  server: {
    proxy: {
      "/api/evals": {
        target: process.env.NANOCODEX_EVALS_API ?? "http://127.0.0.1:8788",
      },
    },
    fs: {
      allow: [repositoryRoot],
    },
  },
});
