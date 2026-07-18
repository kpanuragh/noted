"use client";

import { useCallback } from "react";
import Link from "next/link";
import { api } from "@/lib/api";
import { usePanelData } from "@/lib/usePanelData";
import { formatRelativeTime } from "@/lib/time";
import styles from "./dashboard.module.css";

const DEFAULT_LIMIT = 8;

/**
 * "Recently edited", ordered by true last-edit time.
 *
 * Owns its own loading/error state rather than letting the dashboard hold one
 * shared state: `/api/pages/recent` and the stats endpoint fail independently
 * (one may 404 while the other is fine), and a single failure must degrade one
 * panel, never blank the whole landing page.
 */
export function RecentPages({
  workspaceId,
  limit = DEFAULT_LIMIT,
}: {
  workspaceId: string;
  limit?: number;
}) {
  const load = useCallback(
    () => api.recentPages(workspaceId, limit),
    [workspaceId, limit],
  );
  const { state, retry } = usePanelData(load);

  return (
    <section className={styles.panel} aria-labelledby="recent-heading">
      <h2 id="recent-heading" className={styles.panelTitle}>
        Recently edited
      </h2>

      {state.status === "failed" ? (
        <>
          {/* role="alert" so a screen reader is told the panel failed; without
              it the error replaces the list silently and is only discoverable
              by re-reading the region. */}
          <p className={styles.error} role="alert">
            Couldn&apos;t load your recent pages. The workspace itself is fine — this
            panel just couldn&apos;t reach the server.
          </p>
          <button type="button" className={styles.retry} onClick={retry}>
            Try again
          </button>
        </>
      ) : state.status === "loading" ? (
        <p className={styles.muted}>Loading…</p>
      ) : state.data.length === 0 ? (
        <p className={styles.empty}>
          Nothing here yet. Create your first page and it will show up here as you
          work.
        </p>
      ) : (
        <ul className={styles.list}>
          {state.data.map((page) => (
            <li key={page.id} className={styles.listItem}>
              <Link href={`/pages/${page.id}`} className={styles.pageLink}>
                <span className={styles.pageTitle}>{page.title || "Untitled"}</span>
                <span className={styles.pageTime}>
                  {formatRelativeTime(page.updated_at)}
                </span>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
