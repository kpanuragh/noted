"use client";

import { useEffect, useState } from "react";
import { api } from "@/lib/api";

/**
 * An editable page title.
 *
 * Saves on blur and on Enter, not on every keystroke: a rename is a whole-page
 * write that also bumps `updated_at` and reorders the "Recently edited" list, so
 * firing it per character would make the dashboard flicker and put a row in the
 * log for every letter typed.
 */
export function PageTitle({
  pageId,
  initial,
  onRenamed,
}: {
  pageId: string;
  initial: string;
  onRenamed?: (title: string) => void;
}) {
  const [title, setTitle] = useState(initial);
  const [saving, setSaving] = useState(false);

  // The prop is the source of truth when the page CHANGES — without this,
  // navigating between pages leaves the previous page's title in the box.
  useEffect(() => setTitle(initial), [initial, pageId]);

  async function save() {
    // An empty title is stored as "Untitled" rather than as "", so the tree and
    // the recent list never render a zero-width row the user cannot click.
    const next = title.trim() || "Untitled";
    if (next === initial) return;
    setSaving(true);
    try {
      await api.renamePage(pageId, next);
      setTitle(next);
      onRenamed?.(next);
    } catch {
      // Put the old title back rather than leaving the box showing a change
      // that did not persist.
      setTitle(initial);
    } finally {
      setSaving(false);
    }
  }

  return (
    <input
      aria-label="Page title"
      value={title}
      disabled={saving}
      onChange={(e) => setTitle(e.target.value)}
      onBlur={save}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          (e.target as HTMLInputElement).blur();
        }
        if (e.key === "Escape") {
          setTitle(initial);
          (e.target as HTMLInputElement).blur();
        }
      }}
      placeholder="Untitled"
      style={{
        width: "100%",
        border: "none",
        outline: "none",
        background: "transparent",
        color: "inherit",
        font: "inherit",
        fontSize: "2rem",
        fontWeight: 700,
        padding: 0,
        marginBottom: 8,
      }}
    />
  );
}
