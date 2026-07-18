"use client";

import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { api, type Page } from "@/lib/api";
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
  const [pages, setPages] = useState<Page[] | null>(null);
  const [failed, setFailed] = useState(false);
  // Only used to re-trigger the effect on "Try again".
  const [attempt, setAttempt] = useState(0);

  const retry = useCallback(() => {
    setFailed(false);
    setPages(null);
    setAttempt((n) => n + 1);
  }, []);

  useEffect(() => {
    let cancelled = false;
    api
      .recentPages(workspaceId, limit)
      .then((result) => {
        if (!cancelled) setPages(result);
      })
      .catch(() => {
        if (!cancelled) setFailed(true);
      });
    return () => {
      cancelled = true;
    };
  }, [workspaceId, limit, attempt]);

  return (
    <section className={styles.panel} aria-labelledby="recent-heading">
      <h2 id="recent-heading" className={styles.panelTitle}>
        Recently edited
      </h2>

      {failed ? (
        <>
          <p className={styles.error}>
            Couldn&apos;t load your recent pages. The workspace itself is fine — this
            panel just couldn&apos;t reach the server.
          </p>
          <button type="button" className={styles.retry} onClick={retry}>
            Try again
          </button>
        </>
      ) : pages === null ? (
        <p className={styles.muted}>Loading…</p>
      ) : pages.length === 0 ? (
        <p className={styles.empty}>
          Nothing here yet. Create your first page and it will show up here as you
          work.
        </p>
      ) : (
        <ul className={styles.list}>
          {pages.map((page) => (
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
