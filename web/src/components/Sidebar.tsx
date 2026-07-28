"use client";

import { useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { PageTree } from "@/components/PageTree";
import { api } from "@/lib/api";
import s from "@/components/ui.module.css";

/**
 * The app's one navigation surface, shared by every page.
 *
 * Before this, the dashboard had a brand and quick actions but a page view had
 * only a bare list of titles — no way home, no search, no "new page" without
 * going back first. Consistency is the point: wherever you are, the brand takes
 * you home, the same three actions are one click away, and the page tree is
 * always there.
 */
export function Sidebar({
  workspaceId,
  refreshKey,
}: {
  workspaceId: string;
  refreshKey?: number;
}) {
  const router = useRouter();
  const [creating, setCreating] = useState(false);

  async function newPage() {
    if (!workspaceId || creating) return;
    setCreating(true);
    try {
      const page = await api.createPage(workspaceId, null, "Untitled");
      router.push(`/pages/${page.id}`);
    } catch {
      setCreating(false);
    }
  }

  return (
    <nav className={s.sidebar}>
      {/* Brand doubles as the home link — the convention every user already
          knows, and the "way back" that was missing. */}
      <Link href="/" className={s.brand} aria-label="Home" style={{ textDecoration: "none" }}>
        <span className={s.brandMark}>◆─</span>
        <span className={s.brandName}>noted</span>
      </Link>

      <div className={s.sideNav}>
        <button
          className={s.sideAction}
          onClick={newPage}
          disabled={!workspaceId || creating}
        >
          <span className={s.sideGlyph}>＋</span>
          {creating ? "Creating…" : "New page"}
        </button>
        <Link href="/search" className={s.sideLink}>
          <span className={s.sideGlyph}>⌕</span> Search
        </Link>
        <Link href="/ask" className={s.sideLink}>
          <span className={s.sideGlyph}>◆</span> Ask your notes
        </Link>
      </div>

      <div className={s.sideTree}>
        <p className={s.eyebrow} style={{ marginBottom: 10 }}>
          Pages
        </p>
        {workspaceId && (
          <PageTree
            workspaceId={workspaceId}
            refreshKey={refreshKey}
            onSelect={(p) => router.push(`/pages/${p.id}`)}
          />
        )}
      </div>
    </nav>
  );
}
