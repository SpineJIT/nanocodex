import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react()],
  publicDir: false,
  build: {
    outDir: "dist-evals",
    emptyOutDir: true,
    rollupOptions: { input: "evals.html" },
  },
  server: {
    proxy: {
      "/api/evals": {
        target: process.env.NANOCODEX_EVALS_API ?? "http://127.0.0.1:8788",
      },
    },
  },
});
