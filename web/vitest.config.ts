import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // e2e/ contains Playwright specs, which use a different test() global
    // and must not be collected by vitest.
    exclude: ["**/node_modules/**", "**/e2e/**"],
  },
});
