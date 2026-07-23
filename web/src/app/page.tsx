"use client";

import { useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { PageTree } from "@/components/PageTree";
import { RecentPages } from "@/components/RecentPages";
import { WorkspaceStatsPanel } from "@/components/WorkspaceStatsPanel";
import { PanelBoundary } from "@/components/PanelBoundary";
import { useWorkspace } from "@/lib/useWorkspace";
import { api } from "@/lib/api";
import s from "@/components/ui.module.css";

export default function Home() {
  const router = useRouter();
  const ws = useWorkspace();
  const workspaceId = ws.status === "ready" ? ws.current : "";
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function newPage() {
    if (!workspaceId || creating) return;
    setCreating(true);
    setError(null);
    try {
      const page = await api.createPage(workspaceId, null, "Untitled");
      router.push(`/pages/${page.id}`);
    } catch {
      setError("Couldn't create a page. The API may be unreachable.");
      setCreating(false);
    }
  }

  return (
    <div className={s.app}>
      <nav className={s.sidebar}>
        <div className={s.brand}>
          <span className={s.brandMark}>◆─</span>
          <span className={s.brandName}>noted</span>
        </div>
        <div>
          <p className={s.eyebrow} style={{ marginBottom: 10 }}>
            Pages
          </p>
          {workspaceId && (
            <PageTree
              workspaceId={workspaceId}
              onSelect={(p) => router.push(`/pages/${p.id}`)}
            />
          )}
        </div>
      </nav>

      <main className={s.main}>
        <header style={{ marginBottom: 28 }}>
          <h1 style={{ marginBottom: 8 }}>Your workspace</h1>
          <p className={s.lede}>
            Pick up where you left off, or ask your notes a question.
          </p>
        </header>

        <div className={s.actions} style={{ marginBottom: 32 }}>
          <button className={s.button} onClick={newPage} disabled={!workspaceId || creating}>
            {creating ? "Creating…" : "New page"}
          </button>
          <Link href="/ask" className={s.buttonQuiet} style={{ textDecoration: "none" }}>
            Ask your notes
          </Link>
          <Link href="/search" className={s.buttonQuiet} style={{ textDecoration: "none" }}>
            Search
          </Link>
        </div>

        {error && (
          <p role="alert" className={s.error} style={{ marginBottom: 24 }}>
            {error}
          </p>
        )}

        <div className={s.panels}>
          <PanelBoundary title="Recently edited">
            {workspaceId && <RecentPages workspaceId={workspaceId} />}
          </PanelBoundary>
          <PanelBoundary title="Your knowledge base">
            {workspaceId && <WorkspaceStatsPanel workspaceId={workspaceId} />}
          </PanelBoundary>
        </div>
      </main>
    </div>
  );
}
