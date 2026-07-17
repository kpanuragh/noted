import { test, expect } from "@playwright/test";

// The acceptance test for M1c: a user can find a page by typing part of its name.
test("quick find navigates to a page by title", async ({ page, request }) => {
  const ws = process.env.NEXT_PUBLIC_WORKSPACE_ID!;
  const title = `Findable ${Date.now()}`;
  await request.post("http://localhost:8787/api/pages", {
    data: { workspace_id: ws, title },
  });

  await page.goto("/");
  await page.keyboard.press("Control+k");
  await page.getByPlaceholder(/search/i).fill(title.slice(0, 8));
  await expect(page.getByText(title)).toBeVisible();
  await page.getByText(title).click();
  await expect(page.locator(".ProseMirror")).toBeVisible();
});

// The acceptance test for M1c's headline feature: hybrid search reaches a
// page by CONTENT, not title, from the dedicated /search page.
//
// The phrase below is deliberately a distinctive exact word so the LEXICAL
// arm of `hybrid` alone finds it. Hybrid search's vector arm only fires once
// a page's chunks have been embedded, which requires the `noted-index`
// worker to have run — something this e2e run cannot guarantee. So this test
// proves the hybrid *path and UI* work end-to-end (query -> /api/search ->
// results list -> navigate to editor); the vector arm's own contribution
// (finding content with no verbatim overlap) is exercised by the Rust
// `hybrid` tests in crates/noted-db, not here.
test("hybrid search finds a page by body content and opens it", async ({ page, request }) => {
  const ws = process.env.NEXT_PUBLIC_WORKSPACE_ID!;
  const title = `Untitled ${Date.now()}`;
  const phrase = `zephyroquartz${Date.now()}`;
  const created = await request.post("http://localhost:8787/api/pages", {
    data: { workspace_id: ws, title },
  });
  const { id } = await created.json();

  await page.goto(`/pages/${id}`);
  const editor = page.locator(".ProseMirror");
  await editor.click();
  await editor.type(`this page is about ${phrase} and nothing else`);
  await expect(editor).toContainText(phrase);

  // Give the sync round trip + debounced projection/rechunk time to land.
  await page.waitForTimeout(1000);

  await page.goto("/search");
  await page.getByPlaceholder(/search/i).fill(phrase);
  await expect(page.getByText(title)).toBeVisible();
  await page.getByText(title).click();
  await expect(page.locator(".ProseMirror")).toBeVisible();
});
