"use client";

import { useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { PageTree } from "@/components/PageTree";
import { RecentPages } from "@/components/RecentPages";
import { WorkspaceStatsPanel } from "@/components/WorkspaceStatsPanel";
import { api } from "@/lib/api";
import styles from "@/components/dashboard.module.css";

const WORKSPACE_ID = process.env.NEXT_PUBLIC_WORKSPACE_ID ?? "";

/**
 * The workspace dashboard: what you were last working on, and what noted has
 * learned from it. Each panel fetches and fails independently — see
 * RecentPages / WorkspaceStatsPanel — so a backend endpoint that is down or
 * not yet deployed degrades one card instead of blanking the landing page.
 */
export default function Home() {
  const router = useRouter();
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState(false);

  async function handleNewPage() {
    setCreating(true);
    setCreateError(false);
    try {
      const page = await api.createPage(WORKSPACE_ID, null, "Untitled");
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
          workspaceId={WORKSPACE_ID}
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

        <div className={styles.panels}>
          <RecentPages workspaceId={WORKSPACE_ID} />
          <WorkspaceStatsPanel workspaceId={WORKSPACE_ID} />
        </div>
      </div>
    </main>
  );
}
