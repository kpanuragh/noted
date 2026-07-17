import { test, expect } from "@playwright/test";

// This is the M1a acceptance test: type, reload, content survives.
// It exercises Tiptap -> Yjs -> WebSocket -> yrs -> Postgres and back.
test("typed content persists across a reload", async ({ page, request }) => {
  const created = await request.post("http://127.0.0.1:8080/api/pages", {
    data: {
      workspace_id: process.env.NEXT_PUBLIC_WORKSPACE_ID,
      title: "Persistence test",
    },
  });
  const { id } = await created.json();

  await page.goto(`/pages/${id}`);
  const editor = page.locator(".ProseMirror");
  await editor.click();
  await editor.type("survives a reload");
  await expect(editor).toContainText("survives a reload");

  // Give the sync round trip time to land in Postgres.
  await page.waitForTimeout(1000);
  await page.reload();

  await expect(page.locator(".ProseMirror")).toContainText("survives a reload");
});
