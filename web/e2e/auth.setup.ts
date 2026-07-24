import { test as setup, expect } from "@playwright/test";
import { writeFileSync } from "node:fs";
import { AUTH_FILE, WORKSPACE_FILE } from "./helpers";

// Authenticates ONCE for the integration specs and saves the session, so
// editor.spec and search.spec — which create pages through the real API — run
// as a real signed-in user instead of getting 401.
//
// They also used to hardcode NEXT_PUBLIC_WORKSPACE_ID (the seeded default
// workspace), which a freshly signed-up user is NOT a member of — a 403 on top
// of the 401. A new signup gets its OWN workspace; we discover its id here and
// hand it to the specs via a file, so they post to a workspace they can
// actually write to.

const API = "http://localhost:8787";

setup("authenticate", async ({ request }) => {
  // A fresh user per run, so a re-run never collides with an existing email.
  const email = `e2e-${Date.now()}@example.com`;
  const res = await request.post(`${API}/api/auth/signup`, {
    data: { email, password: "e2e-password-123456", display_name: "E2E" },
  });
  expect(
    res.ok(),
    `signup failed (${res.status()}); is the API running on :8787?`,
  ).toBeTruthy();

  // The signup created the user's own workspace; use it, not the default.
  const wsRes = await request.get(`${API}/api/workspaces`);
  expect(wsRes.ok()).toBeTruthy();
  const workspaces = (await wsRes.json()) as Array<{ id: string }>;
  expect(workspaces.length, "a new user should have exactly one workspace").toBeGreaterThan(0);
  writeFileSync(WORKSPACE_FILE, JSON.stringify({ id: workspaces[0].id }));

  // Persist the session cookie for both the browser and the API request
  // context in the dependent project.
  await request.storageState({ path: AUTH_FILE });
});
