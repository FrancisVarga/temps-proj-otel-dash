import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Temps serves the embedded UI at /api/x/otel-dash/ui/ — `base` must match
// exactly (trailing slash included) or hashed asset URLs 404.
export default defineConfig({
  plugins: [react()],
  base: "/api/x/otel-dash/ui/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    target: "es2020",
  },
  server: {
    port: 5176,
    proxy: {
      // Point this at a running Temps instance for live data in `bun run dev`.
      "/api/x/otel-dash": {
        target: process.env.TEMPS_URL ?? "http://localhost:8081",
        changeOrigin: true,
      },
    },
  },
});
