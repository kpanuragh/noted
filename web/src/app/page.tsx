"use client";

import { useState } from "react";
import { useWorkspace } from "@/lib/useWorkspace";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { PageTree } from "@/components/PageTree";
import { RecentPages } from "@/components/RecentPages";
import { WorkspaceStatsPanel } from "@/components/WorkspaceStatsPanel";
import { PanelBoundary } from "@/components/PanelBoundary";
import { api } from "@/lib/api";
import styles from "@/components/dashboard.module.css";



/**
 * The workspace dashboard: what you were last working on, and what noted has
 * learned from it. Each panel fetches and fails independently — see
 * RecentPages / WorkspaceStatsPanel — so a backend endpoint that is down or
 * not yet deployed degrades one card instead of blanking the landing page.
 */
export default function Home() {
  // Which workspace this is depends on WHO IS SIGNED IN, so it is asked for at
  // runtime rather than baked in at build time. See `useWorkspace`.
  const ws = useWorkspace();
  const workspaceId = ws.status === "ready" ? ws.current : "";
  const router = useRouter();
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState(false);

  async function handleNewPage() {
    setCreating(true);
    setCreateError(false);
    try {
      const page = await api.createPage(workspaceId, null, "Untitled");
      router.push(`/pages/${page.id}`);
    } catch {
      setCreateError(true);
      setCreating(false);
    }
  }

  return (
    <main className={styles.layout}>
      <nav className={styles.sidebar} aria-label="Pages">
        <h2 className={styles.sidebarHeading}>Pages</h2>
        <PageTree
          workspaceId={workspaceId}
          onSelect={(p) => router.push(`/pages/${p.id}`)}
        />
      </nav>

      <div className={styles.main}>
        <h1 className={styles.title}>Your workspace</h1>
        <p className={styles.subtitle}>
          Pick up where you left off, or ask your notes a question.
        </p>

        <div className={styles.actions}>
          <button
            type="button"
            className={styles.primaryAction}
            onClick={handleNewPage}
            disabled={creating}
          >
            {creating ? "Creating…" : "New page"}
          </button>
          <Link href="/search" className={styles.secondaryAction}>
            Search your notes
          </Link>
        </div>

        {createError && (
          <p className={styles.error} role="alert" style={{ marginBottom: 20 }}>
            Couldn&apos;t create the page. Check that the noted server is running and
            try again.
          </p>
        )}

        {/* One boundary per panel, not one around both: a shared boundary
            would let either panel's crash take out the other, which is the
            blanking this is meant to stop. */}
        <div className={styles.panels}>
          <PanelBoundary title="Recently edited">
            <RecentPages workspaceId={workspaceId} />
          </PanelBoundary>
          <PanelBoundary title="Your knowledge base">
            <WorkspaceStatsPanel workspaceId={workspaceId} />
          </PanelBoundary>
        </div>
      </div>
    </main>
  );
}
