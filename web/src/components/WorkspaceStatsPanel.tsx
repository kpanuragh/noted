"use client";

import { useCallback } from "react";
import { api, type WorkspaceStats } from "@/lib/api";
import { usePanelData } from "@/lib/usePanelData";
import { formatCount } from "@/lib/time";
import styles from "./dashboard.module.css";

/**
 * The four workspace counters, told as a sentence rather than a debug dump.
 *
 * The graph numbers lead because the graph is what noted does that a plain
 * notes app does not; "412 entities across 1,204 connections" is a claim about
 * the user's knowledge, whereas "edges: 1204" is a claim about our schema.
 *
 * Like RecentPages, this owns its own failure state so a 404 here leaves the
 * rest of the dashboard intact.
 */
export function WorkspaceStatsPanel({ workspaceId }: { workspaceId: string }) {
  const load = useCallback(() => api.workspaceStats(workspaceId), [workspaceId]);
  const { state, retry } = usePanelData(load);

  return (
    <section className={styles.panel} aria-labelledby="stats-heading">
      <h2 id="stats-heading" className={styles.panelTitle}>
        Your knowledge base
      </h2>

      {state.status === "failed" ? (
        <>
          {/* role="alert": see RecentPages. */}
          <p className={styles.error} role="alert">
            Workspace insights are unavailable right now. Your pages are unaffected.
          </p>
          <button type="button" className={styles.retry} onClick={retry}>
            Try again
          </button>
        </>
      ) : state.status === "loading" ? (
        <p className={styles.muted}>Loading…</p>
      ) : (
        <>
          <StatsHeadline stats={state.data} />
          <div className={styles.statRow}>
            <span className={styles.statLabel}>Pages</span>
            <span className={styles.statValue}>{formatCount(state.data.pages)}</span>
          </div>
          <div className={styles.statRow}>
            <span className={styles.statLabel}>Passages searchable</span>
            <span className={styles.statValue}>
              {formatCount(state.data.chunks_indexed)}
            </span>
          </div>
          <div className={styles.statRow}>
            <span className={styles.statLabel}>Things noted knows about</span>
            <span className={styles.statValue}>{formatCount(state.data.entities)}</span>
          </div>
          <div className={styles.statRow}>
            <span className={styles.statLabel}>Connections between them</span>
            <span className={styles.statValue}>{formatCount(state.data.edges)}</span>
          </div>
        </>
      )}
    </section>
  );
}

function StatsHeadline({ stats }: { stats: WorkspaceStats }) {
  if (stats.pages === 0) {
    return (
      <p className={styles.empty}>
        Nothing indexed yet. Write your first page and noted will make it
        searchable and start mapping how its ideas connect.
      </p>
    );
  }

  if (stats.entities === 0) {
    return (
      <p className={styles.empty}>
        Still reading your pages. noted extracts entities and their connections in
        the background — this fills in shortly after you write.
      </p>
    );
  }

  return (
    <p className={styles.graphHeadline}>
      noted has found{" "}
      <span className={styles.graphNumber}>{formatCount(stats.entities)}</span>{" "}
      {stats.entities === 1 ? "thing" : "things"} in your notes, linked by{" "}
      <span className={styles.graphNumber}>{formatCount(stats.edges)}</span>{" "}
      {stats.edges === 1 ? "connection" : "connections"}.
    </p>
  );
}
