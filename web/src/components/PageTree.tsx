"use client";

import { useEffect, useState } from "react";
import { api, type Page } from "@/lib/api";
import s from "@/components/ui.module.css";

/**
 * The workspace's page tree.
 *
 * A list of buttons rather than links, because selection is the parent's to
 * route — the dashboard and the editor put a selected page in different places.
 */
export function PageTree({
  workspaceId,
  onSelect,
  /** Bump to refetch. A rename changes a title the tree is already showing, and
   *  without this the sidebar keeps saying "Untitled" until a full reload. */
  refreshKey = 0,
}: {
  workspaceId: string;
  onSelect: (page: Page) => void;
  refreshKey?: number;
}) {
  const [pages, setPages] = useState<Page[]>([]);

  useEffect(() => {
    if (!workspaceId) return;
    let cancelled = false;
    api
      .listPages(workspaceId)
      .then((p) => {
        if (!cancelled) setPages(p);
      })
      .catch(() => {
        // The tree is navigation, not content: a failure here should leave the
        // rest of the page usable rather than take it down.
      });
    return () => {
      cancelled = true;
    };
  }, [workspaceId, refreshKey]);

  if (pages.length === 0) {
    return <p className={s.empty}>No pages yet.</p>;
  }

  return (
    <ul className={s.list}>
      {pages.map((p) => (
        <li key={p.id}>
          <button type="button" className={s.treeItem} onClick={() => onSelect(p)}>
            {p.title || "Untitled"}
          </button>
        </li>
      ))}
    </ul>
  );
}
