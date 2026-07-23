import { test, expect, type Page as PWPage } from "@playwright/test";

const WS = process.env.NEXT_PUBLIC_WORKSPACE_ID ?? "00000000-0000-0000-0000-000000000001";
const API = "http://localhost:8787";

type Stub = { status?: number; body?: unknown };

/**
 * Stub the three endpoints the dashboard reads.
 *
 * The dashboard's whole contract is "each panel fails independently", and the
 * only way to test that honestly is to fail one endpoint at a time — which a
 * live backend will not do on demand. The integration test at the bottom
 * covers the real wiring; these cover the states the real backend can't be
 * asked to produce.
 */
async function stubApi(
  page: PWPage,
  stubs: { tree?: Stub; recent?: Stub; stats?: Stub },
) {
  const fulfill = async (route: import("@playwright/test").Route, stub: Stub) => {
    if (stub.status && stub.status >= 400) {
      await route.fulfill({ status: stub.status, body: "" });
      return;
    }
    // `"body" in stub` rather than `stub.body ?? []`, so a stub can serve a
    // literal `null` body — one of the malformed shapes under test, which
    // `??` would silently rewrite into a valid empty list.
    const body = "body" in stub ? stub.body : [];
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(body),
    });
  };

  // The workspace list. The dashboard resolves which workspace it is looking at
  // from the server rather than from a build-time constant, so without this the
  // page has no workspace and renders no panels at all.
  await page.route(`${API}/api/workspaces`, (r) =>
    r.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify([{ id: WS, name: "Test workspace", role: "owner" }]),
    }),
  );
  await page.route(`${API}/api/pages/recent*`, (r) => fulfill(r, stubs.recent ?? {}));
  await page.route(`${API}/api/workspaces/*/stats`, (r) => fulfill(r, stubs.stats ?? {}));
  // Must be registered last: `**/api/pages*` also matches /api/pages/recent,
  // and Playwright matches the most recently registered route first.
  await page.route(`${API}/api/pages?*`, (r) => fulfill(r, stubs.tree ?? { body: [] }));
}

function page_(id: string, title: string, updatedAt: string) {
  return {
    id,
    workspace_id: WS,
    parent_id: null,
    title,
    created_at: "2020-01-01T00:00:00Z",
    updated_at: updatedAt,
  };
}

test("shows recent pages with relative edit times and links to them", async ({ page }) => {
  const twoHoursAgo = new Date(Date.now() - 2 * 3600 * 1000).toISOString();
  await stubApi(page, {
    recent: { body: [page_("p-1", "Quarterly plan", twoHoursAgo)] },
    stats: { body: { pages: 3, chunks_indexed: 40, entities: 412, edges: 1204 } },
  });

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Recently edited" })).toBeVisible();
  await expect(page.getByText("Quarterly plan")).toBeVisible();
  await expect(page.getByText("2 hours ago")).toBeVisible();

  await page.getByRole("link", { name: /Quarterly plan/ }).click();
  await expect(page).toHaveURL(/\/pages\/p-1$/);
});

test("presents the graph stats in user language, with grouped numbers", async ({ page }) => {
  await stubApi(page, {
    recent: { body: [] },
    stats: { body: { pages: 3, chunks_indexed: 40, entities: 412, edges: 1204 } },
  });

  await page.goto("/");

  const stats = page.getByRole("region", { name: "Your knowledge base" });
  await expect(stats).toContainText("412");
  await expect(stats).toContainText("1,204");
  await expect(stats).toContainText(/connections/);
  // The schema words must not leak into the UI.
  await expect(stats).not.toContainText("edges");
  await expect(stats).not.toContainText("chunks_indexed");
});

test("a brand-new workspace gets guidance, not a blank dashboard", async ({ page }) => {
  await stubApi(page, {
    recent: { body: [] },
    stats: { body: { pages: 0, chunks_indexed: 0, entities: 0, edges: 0 } },
  });

  await page.goto("/");

  await expect(page.getByText(/Nothing here yet/i)).toBeVisible();
  await expect(page.getByText(/Nothing indexed yet/i)).toBeVisible();
  await expect(page.getByRole("button", { name: "New page" })).toBeVisible();
});

test("a 404 on one panel leaves the rest of the dashboard working", async ({ page }) => {
  // The exact situation while the stats endpoint is still being built.
  await stubApi(page, {
    recent: { body: [page_("p-9", "Still fine", new Date().toISOString())] },
    stats: { status: 404 },
  });

  await page.goto("/");

  await expect(page.getByText(/insights are unavailable/i)).toBeVisible();
  // The rest of the page survived.
  await expect(page.getByText("Still fine")).toBeVisible();
  await expect(page.getByRole("button", { name: "New page" })).toBeVisible();
  await expect(page.getByRole("link", { name: /Search/ })).toBeVisible();
});

test("both panels failing still leaves usable quick actions", async ({ page }) => {
  await stubApi(page, {
    tree: { status: 500 },
    recent: { status: 500 },
    stats: { status: 500 },
  });

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Your workspace" })).toBeVisible();
  await expect(page.getByRole("button", { name: "New page" })).toBeVisible();
  await expect(page.getByText(/Couldn't load your recent pages/i)).toBeVisible();
  await expect(page.getByText(/insights are unavailable/i)).toBeVisible();
});

test("retry re-requests a panel that failed", async ({ page }) => {
  // This test drives its own routes rather than `stubApi`, so it needs the
  // workspace list too — without it the dashboard has no workspace and renders
  // no panels to retry.
  await page.route(`${API}/api/workspaces`, (r) =>
    r.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify([{ id: WS, name: "Test workspace", role: "owner" }]),
    }),
  );
  // The route serves 500 until the test says otherwise, rather than counting
  // calls. Counting is not a stable proxy for "the user clicked retry": React
  // StrictMode double-invokes effects in dev, so the panel fetches twice on
  // mount and a call-counting stub would hand it a success before anyone
  // touched the button — which is exactly how this test started passing for the
  // wrong reason and then failing for the right one.
  let recovered = false;
  let callsAfterRetry = 0;
  await page.route(`${API}/api/workspaces/*/stats`, async (route) => {
    if (!recovered) {
      await route.fulfill({ status: 500, body: "" });
      return;
    }
    callsAfterRetry += 1;
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ pages: 1, chunks_indexed: 2, entities: 3, edges: 4 }),
    });
  });
  // `/api/pages*` does NOT match `/api/pages/recent` — Playwright's `*` does
  // not cross a `/`. Left unstubbed, recent-pages hits the real API, gets a
  // 401, and the app correctly redirects to /signin, so the panel under test
  // never renders at all.
  await page.route(`${API}/api/pages/recent*`, (r) =>
    r.fulfill({ status: 200, contentType: "application/json", body: "[]" }),
  );
  await page.route(`${API}/api/pages*`, (r) =>
    r.fulfill({ status: 200, contentType: "application/json", body: "[]" }),
  );

  await page.goto("/");

  await expect(page.getByText(/insights are unavailable/i)).toBeVisible();

  recovered = true;
  await page
    .getByRole("region", { name: "Your knowledge base" })
    .getByRole("button", { name: "Try again" })
    .click();

  await expect(page.getByText(/insights are unavailable/i)).toHaveCount(0);
  await expect(page.getByRole("region", { name: "Your knowledge base" })).toContainText("3");
  expect(callsAfterRetry).toBeGreaterThan(0);
});

/**
 * Malformed 200s — API version skew.
 *
 * These are the case the per-panel `.catch()` could not cover. A 200 with a
 * wrong-shaped body used to pass the fetch layer's `as T` cast untouched and
 * throw during render, and React unmounts the whole tree on an uncaught render
 * error, so a single skewed endpoint blanked the entire dashboard. Each test
 * below asserts the *other* panel and the quick actions survived, because
 * "shows an error" is only half the contract — the other half is that the
 * failure stayed inside one panel.
 */
test("a wrong-shaped 200 degrades one panel instead of blanking the dashboard", async ({
  page,
}) => {
  // An object where the client expects a list: what a "wrap the response in an
  // envelope" backend change looks like from here.
  await stubApi(page, {
    recent: { body: { pages: [], total: 0 } },
    stats: { body: { pages: 3, chunks_indexed: 40, entities: 412, edges: 1204 } },
  });

  await page.goto("/");

  // Order matters. The panel error only appears once the bad response has been
  // received and handled, so waiting for it first guarantees everything below
  // is observed AFTER the moment the page used to blank. Asserting the heading
  // first passed vacuously against the unfixed code: it was still true during
  // the brief window before the fetch resolved and the render threw.
  const recentError = page.getByText(/Couldn't load your recent pages/i);
  await expect(recentError).toBeVisible();
  // ...and the failure is announced, not just shown.
  await expect(recentError).toHaveAttribute("role", "alert");

  // Only now: the page itself survived the malformed payload.
  await expect(page.getByRole("heading", { name: "Your workspace" })).toBeVisible();
  await expect(page.getByRole("button", { name: "New page" })).toBeVisible();

  // The good panel is unaffected.
  await expect(page.getByRole("region", { name: "Your knowledge base" })).toContainText(
    "1,204",
  );
});

test("a 200 with a null body shows an error instead of loading forever", async ({
  page,
}) => {
  // `null` used to be both the not-loaded sentinel and a decodable body, so
  // this hung on "Loading…" with nothing to retry.
  await stubApi(page, {
    recent: { body: [page_("p-2", "Still fine", new Date().toISOString())] },
    stats: { body: null },
  });

  await page.goto("/");

  const stats = page.getByRole("region", { name: "Your knowledge base" });
  const statsError = page.getByText(/insights are unavailable/i);
  await expect(statsError).toBeVisible();
  await expect(statsError).toHaveAttribute("role", "alert");
  await expect(stats).not.toContainText("Loading");
  // Recoverable, unlike the hang it replaces.
  await expect(stats.getByRole("button", { name: "Try again" })).toBeVisible();

  await expect(page.getByText("Still fine")).toBeVisible();
});

test("a list of wrong-shaped elements degrades only its own panel", async ({ page }) => {
  // Individually plausible objects missing the fields the UI reads: the panel
  // would otherwise render "unknown" times and empty titles rather than fail.
  await stubApi(page, {
    recent: { body: [{ id: "p-1", name: "wrong field names" }] },
    stats: { body: { pages: 3, chunks_indexed: 40, entities: 412, edges: 1204 } },
  });

  await page.goto("/");

  // Settle on the failure first — see the ordering note above.
  await expect(page.getByText(/Couldn't load your recent pages/i)).toBeVisible();
  await expect(page.getByRole("region", { name: "Your knowledge base" })).toContainText(
    "412",
  );
  await expect(page.getByRole("heading", { name: "Your workspace" })).toBeVisible();
});

// Integration: real backend, no stubs. Skipped when the API is not up, because
// a test that cannot reach the server proves nothing by "passing".
test("dashboard reflects a page created through the real API", async ({ page, request }) => {
  let reachable = false;
  try {
    const probe = await request.get(`${API}/api/pages?workspace_id=${WS}`, { timeout: 2000 });
    reachable = probe.ok();
  } catch {
    reachable = false;
  }
  test.skip(!reachable, "noted API not running on :8787");

  const title = `Dashboard ${Date.now()}`;
  await request.post(`${API}/api/pages`, { data: { workspace_id: WS, title } });

  await page.goto("/");

  // Scope to the Recently edited panel. The sidebar tree renders the same
  // title, so an unscoped getByText matches twice and trips strict mode —
  // and the panel is what this test is actually about.
  const recent = page.getByRole("region", { name: "Recently edited" });
  await expect(recent.getByText(title)).toBeVisible();
  await recent.getByRole("link", { name: new RegExp(title) }).click();
  await expect(page.locator(".ProseMirror")).toBeVisible();
});
