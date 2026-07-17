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
