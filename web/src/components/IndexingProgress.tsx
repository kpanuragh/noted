"use client";

import { useEffect, useState } from "react";
import { api, type IndexingStatus } from "@/lib/api";
import s from "@/components/ui.module.css";

/** How often to re-check while work is outstanding. */
const POLL_MS = 4000;

type Stage = { label: string; done: number; total: number };

function stages(st: IndexingStatus): Stage[] {
  return [
    { label: "Passages indexed", done: st.embedded, total: st.embed_total },
    { label: "Connections found", done: st.extracted, total: st.extract_total },
    { label: "Themes summarised", done: st.summarised, total: st.summary_total },
  ];
}

/**
 * What the indexer is still working through, for a workspace.
 *
 * Shown only while there is outstanding work. A note becomes searchable, then
 * joins the graph, then its theme gets summarised — three stages at very
 * different speeds, and until now a surface that depended on a later one just
 * looked empty. "No themes yet" is true but unhelpful; "8 of 33 themes
 * summarised" is the same fact with the reason attached.
 *
 * `only` narrows it to the stage a surface actually depends on — the Ask page's
 * global mode is blocked on summaries specifically, and listing the two stages
 * it does not care about would bury that.
 */
export function IndexingProgress({
  workspaceId,
  only,
}: {
  workspaceId: string;
  only?: "summary";
}) {
  const [status, setStatus] = useState<IndexingStatus | null>(null);

  useEffect(() => {
    if (!workspaceId) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;

    const tick = async () => {
      try {
        const next = await api.indexing(workspaceId);
        if (cancelled) return;
        setStatus(next);
        // Stop polling once there is nothing left to report. Indexing is not a
        // live dashboard; a workspace at rest should cost nothing.
        const pending =
          next.embed_total - next.embedded +
          (next.extract_total - next.extracted) +
          (next.summary_total - next.summarised);
        if (pending > 0) timer = setTimeout(tick, POLL_MS);
      } catch {
        // Progress is supplementary. If it cannot be fetched, the surface it
        // annotates still works, so this fails silently rather than showing an
        // error about the thing that was only ever there to explain one.
      }
    };
    tick();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [workspaceId]);

  if (!status) return null;

  const shown = (only === "summary"
    ? stages(status).filter((g) => g.label === "Themes summarised")
    : stages(status)
  ).filter((g) => g.total > 0 && g.done < g.total);

  if (shown.length === 0) return null;

  return (
    <div className={s.progress} role="status" aria-live="polite">
      <p className={s.progressLede}>
        Still indexing. This runs in the background — you can keep writing.
      </p>
      {shown.map((g) => {
        const pct = Math.round((g.done / g.total) * 100);
        return (
          <div key={g.label} className={s.progressRow}>
            <div className={s.progressHead}>
              <span>{g.label}</span>
              <span className={s.rowMeta}>
                {g.done} of {g.total}
              </span>
            </div>
            <div
              className={s.progressTrack}
              role="progressbar"
              aria-valuenow={g.done}
              aria-valuemin={0}
              aria-valuemax={g.total}
              aria-label={g.label}
            >
              <div className={s.progressFill} style={{ width: `${pct}%` }} />
            </div>
          </div>
        );
      })}
    </div>
  );
}
