import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// Component tests run in jsdom; native commands are mocked per test, so no
// Tauri runtime is involved. The `test` field is Vitest-only and does not
// affect the Vite dev/build pipelines in vite.config.ts.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    // Component tests assert their CSS modules' contract via `?raw` imports;
    // without this Vitest stubs CSS imports as empty modules.
    css: true,
  },
});
