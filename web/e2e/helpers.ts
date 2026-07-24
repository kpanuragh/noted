import { readFileSync } from "node:fs";

// Shared paths, kept in this NON-test module so both the setup and the specs
// can import them. Playwright forbids a spec importing a test file, so these
// cannot live in auth.setup.ts.
export const AUTH_FILE = "e2e/.auth/user.json";
export const WORKSPACE_FILE = "e2e/.auth/workspace.json";

/**
 * The workspace id the authenticated e2e user actually owns.
 *
 * Written by `auth.setup.ts`. Specs must use this rather than
 * NEXT_PUBLIC_WORKSPACE_ID (the seeded default), which the test user is not a
 * member of — posting there is a 403.
 */
export function testWorkspaceId(): string {
  return JSON.parse(readFileSync(WORKSPACE_FILE, "utf8")).id as string;
}
