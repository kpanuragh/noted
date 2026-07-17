import { defineConfig } from "@playwright/test";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

// Next.js loads .env.local for the dev server, but the Playwright runner is a
// separate process that does not — so specs reading NEXT_PUBLIC_* saw
// `undefined` and the API rejected the request with a confusing
// "missing field workspace_id". Load it here too. Real environment variables
// still win, so CI can override without editing the file.
function loadEnvLocal() {
  let contents: string;
  try {
    contents = readFileSync(resolve(__dirname, ".env.local"), "utf8");
  } catch {
    return; // Absent is fine: the environment may already supply these.
  }
  for (const line of contents.split("\n")) {
    const match = /^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)\s*$/.exec(line);
    if (!match) continue;
    const [, key, rawValue] = match;
    if (process.env[key] !== undefined) continue;
    process.env[key] = rawValue.trim().replace(/^["']|["']$/g, "");
  }
}
loadEnvLocal();

export default defineConfig({
  testDir: "./e2e",
  use: { baseURL: "http://localhost:3000" },
  webServer: {
    command: "npm run dev",
    url: "http://localhost:3000",
    reuseExistingServer: true,
  },
});
