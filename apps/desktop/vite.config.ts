import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const env = (globalThis as { process?: { env?: Record<string, string> } })
  .process?.env;
const daemonTarget = env?.POCKETS_DAEMON_URL ?? "http://127.0.0.1:18786";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    proxy: {
      "/api": {
        target: daemonTarget,
        changeOrigin: false,
      },
      "/auth": {
        target: daemonTarget,
        changeOrigin: false,
      },
    },
  },
  build: {
    target: "es2022",
    outDir: "dist",
  },
});
