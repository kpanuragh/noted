"use client";

import { forwardRef, useEffect, useImperativeHandle, useState } from "react";
import type { Editor, Range } from "@tiptap/core";
import s from "./slashMenu.module.css";

export type SlashItem = {
  title: string;
  hint: string;
  glyph: string;
  /** Words a person might type instead of the title. */
  keywords: string[];
  run: (editor: Editor, range: Range) => void;
};

/**
 * The blocks a note can actually contain.
 *
 * Deliberately only what this editor really supports. A menu that lists things
 * the app cannot do is worse than a short menu: every unimplemented entry is a
 * promise broken at the moment someone tries it.
 */
export const SLASH_ITEMS: SlashItem[] = [
  {
    title: "Text",
    hint: "Plain paragraph",
    glyph: "¶",
    keywords: ["paragraph", "plain", "body"],
    run: (e, r) => e.chain().focus().deleteRange(r).setParagraph().run(),
  },
  {
    title: "Heading 1",
    hint: "Big section heading",
    glyph: "H1",
    keywords: ["title", "large", "h1"],
    run: (e, r) => e.chain().focus().deleteRange(r).setNode("heading", { level: 1 }).run(),
  },
  {
    title: "Heading 2",
    hint: "Medium section heading",
    glyph: "H2",
    keywords: ["subtitle", "h2"],
    run: (e, r) => e.chain().focus().deleteRange(r).setNode("heading", { level: 2 }).run(),
  },
  {
    title: "Heading 3",
    hint: "Small section heading",
    glyph: "H3",
    keywords: ["h3", "minor"],
    run: (e, r) => e.chain().focus().deleteRange(r).setNode("heading", { level: 3 }).run(),
  },
  {
    title: "Bulleted list",
    hint: "An unordered list",
    glyph: "•",
    keywords: ["bullet", "unordered", "ul", "point"],
    run: (e, r) => e.chain().focus().deleteRange(r).toggleBulletList().run(),
  },
  {
    title: "Numbered list",
    hint: "An ordered list",
    glyph: "1.",
    keywords: ["ordered", "ol", "number", "step"],
    run: (e, r) => e.chain().focus().deleteRange(r).toggleOrderedList().run(),
  },
  {
    title: "To-do list",
    hint: "Track tasks with checkboxes",
    glyph: "☑",
    keywords: ["todo", "task", "checkbox", "check"],
    run: (e, r) => e.chain().focus().deleteRange(r).toggleTaskList().run(),
  },
  {
    title: "Table",
    hint: "3 columns, with a header row",
    glyph: "▦",
    keywords: ["table", "grid", "rows", "columns"],
    run: (e, r) =>
      e
        .chain()
        .focus()
        .deleteRange(r)
        .insertTable({ rows: 3, cols: 3, withHeaderRow: true })
        .run(),
  },
  {
    title: "Image",
    hint: "Embed by URL",
    glyph: "▣",
    keywords: ["image", "picture", "photo", "img"],
    run: (e, r) => {
      const src = window.prompt("Image URL");
      // Cancelled or empty: leave the "/" text alone rather than deleting the
      // range for an insert that is not going to happen.
      if (!src || !src.trim()) return;
      e.chain().focus().deleteRange(r).setImage({ src: src.trim() }).run();
    },
  },
  {
    title: "Quote",
    hint: "Set text apart",
    glyph: "❝",
    keywords: ["blockquote", "cite"],
    run: (e, r) => e.chain().focus().deleteRange(r).toggleBlockquote().run(),
  },
  {
    title: "Code block",
    hint: "Monospaced, unformatted",
    glyph: "</>",
    keywords: ["code", "snippet", "pre", "monospace"],
    run: (e, r) => e.chain().focus().deleteRange(r).toggleCodeBlock().run(),
  },
  {
    title: "Divider",
    hint: "A horizontal rule",
    glyph: "—",
    keywords: ["hr", "rule", "separator", "line"],
    run: (e, r) => e.chain().focus().deleteRange(r).setHorizontalRule().run(),
  },
];

/** Matches on title AND keywords, so "bullet" finds "Bulleted list". */
export function filterSlashItems(query: string): SlashItem[] {
  const q = query.trim().toLowerCase();
  if (!q) return SLASH_ITEMS;
  return SLASH_ITEMS.filter(
    (i) =>
      i.title.toLowerCase().includes(q) ||
      i.keywords.some((k) => k.includes(q)),
  );
}

export type SlashMenuHandle = {
  /** Returns true when the key was consumed, so the editor does not also act. */
  onKeyDown: (event: KeyboardEvent) => boolean;
};

export const SlashMenu = forwardRef<
  SlashMenuHandle,
  { items: SlashItem[]; command: (item: SlashItem) => void }
>(function SlashMenu({ items, command }, ref) {
  const [selected, setSelected] = useState(0);

  // A filter change can leave the cursor past the end of a shorter list.
  useEffect(() => setSelected(0), [items]);

  useImperativeHandle(ref, () => ({
    onKeyDown: (event: KeyboardEvent) => {
      if (items.length === 0) return false;
      if (event.key === "ArrowUp") {
        setSelected((i) => (i + items.length - 1) % items.length);
        return true;
      }
      if (event.key === "ArrowDown") {
        setSelected((i) => (i + 1) % items.length);
        return true;
      }
      if (event.key === "Enter") {
        command(items[selected]);
        return true;
      }
      return false;
    },
  }));

  if (items.length === 0) {
    return (
      <div className={s.menu}>
        <p className={s.empty}>No blocks match that.</p>
      </div>
    );
  }

  return (
    <div className={s.menu} role="listbox" aria-label="Insert block">
      {items.map((item, i) => (
        <button
          key={item.title}
          type="button"
          role="option"
          aria-selected={i === selected}
          className={i === selected ? s.itemActive : s.item}
          // Keeps the editor selection alive so the command has a range to act
          // on — the same reason the selection menu's controls do it.
          onMouseDown={(e) => e.preventDefault()}
          onMouseEnter={() => setSelected(i)}
          onClick={() => command(item)}
        >
          <span className={s.glyph}>{item.glyph}</span>
          <span className={s.text}>
            <span className={s.title}>{item.title}</span>
            <span className={s.hint}>{item.hint}</span>
          </span>
        </button>
      ))}
    </div>
  );
});
