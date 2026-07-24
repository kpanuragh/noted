import { test, expect } from "@playwright/test";
import { testWorkspaceId } from "./helpers";

// This is the M1a acceptance test: type, reload, content survives.
// It exercises Tiptap -> Yjs -> WebSocket -> yrs -> Postgres and back.
test("typed content persists across a reload", async ({ page, request }) => {
  const created = await request.post("http://localhost:8787/api/pages", {
    data: {
      workspace_id: testWorkspaceId(),
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
