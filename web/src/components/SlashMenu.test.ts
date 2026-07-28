import { describe, it, expect } from "vitest";
import { getSchema } from "@tiptap/core";
import { SLASH_ITEMS, filterSlashItems } from "./SlashMenu";
import { editorExtensions } from "@/lib/editorExtensions";

describe("slash menu", () => {
  it("matches on keywords, not only on the title", () => {
    // Someone reaching for a to-do list types "todo" or "checkbox", never
    // "To-do list" — a menu that only matches its own labels makes you guess
    // the app's vocabulary.
    expect(filterSlashItems("todo").map((i) => i.title)).toContain("To-do list");
    expect(filterSlashItems("checkbox").map((i) => i.title)).toContain("To-do list");
    expect(filterSlashItems("grid").map((i) => i.title)).toContain("Table");
    expect(filterSlashItems("hr").map((i) => i.title)).toContain("Divider");
  });

  it("returns everything for an empty query and nothing for nonsense", () => {
    expect(filterSlashItems("")).toHaveLength(SLASH_ITEMS.length);
    expect(filterSlashItems("   ")).toHaveLength(SLASH_ITEMS.length);
    expect(filterSlashItems("zzzzz")).toHaveLength(0);
  });

  /**
   * THE RULE THIS MENU LIVES BY.
   *
   * Every entry must correspond to a node the editor's schema actually has.
   * A menu listing blocks the app cannot insert is worse than a short menu:
   * each one is a promise broken at the moment someone tries it, and the
   * failure is silent — the command no-ops and the "/" text just vanishes.
   */
  it("offers only blocks the schema can actually create", () => {
    const schema = getSchema(editorExtensions());
    const required: Record<string, string> = {
      Text: "paragraph",
      "Heading 1": "heading",
      "Bulleted list": "bulletList",
      "Numbered list": "orderedList",
      "To-do list": "taskList",
      Table: "table",
      Image: "image",
      Callout: "callout",
      Toggle: "toggleBlock",
      Video: "youtube",
      Quote: "blockquote",
      "Code block": "codeBlock",
      Divider: "horizontalRule",
    };
    for (const [title, node] of Object.entries(required)) {
      expect(
        SLASH_ITEMS.some((i) => i.title === title),
        `menu is missing "${title}"`,
      ).toBe(true);
      expect(schema.nodes[node], `schema cannot create "${node}" for "${title}"`).toBeTruthy();
    }
  });

  it("gives every entry a distinct title, so filtering cannot be ambiguous", () => {
    const titles = SLASH_ITEMS.map((i) => i.title);
    expect(new Set(titles).size).toBe(titles.length);
  });
});
