import { defineConfig } from "vitest/config";
import { fileURLToPath } from "node:url";

export default defineConfig({
  // The same `@/` alias tsconfig and Next already use. Without it a module is
  // resolvable in the app but not in its own tests, so importing one file into
  // another can break a suite that never mentioned either.
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  test: {
    // e2e/ contains Playwright specs, which use a different test() global
    // and must not be collected by vitest.
    exclude: ["**/node_modules/**", "**/e2e/**"],
  },
});
