"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { api, type RelatedHit } from "@/lib/api";
import s from "@/components/ui.module.css";

export function RelatedNotes({ pageId }: { pageId: string }) {
  const router = useRouter();
  const [related, setRelated] = useState<RelatedHit[]>([]);

  useEffect(() => {
    setRelated([]);
    api
      .related(pageId)
      .then(setRelated)
      .catch(console.error);
  }, [pageId]);

  // An empty "Related" heading is worse than no heading — pages legitimately
  // have no neighbours until the index catches up.
  if (related.length === 0) return null;

  return (
    <aside className={s.related}>
      <div className={s.divider} />
      {/* Amber, because these are GRAPH-DERIVED — the connections the system
          found between notes, not links anyone typed. The one place in the app
          that colour is allowed to mean "the system inferred this". */}
      <p className={s.relatedTitle}>
        <span className={s.relatedGlyph}>◆─</span> Connected notes
      </p>
      <div className={s.relatedList}>
        {related.map((r, i) => (
          <button
            key={r.page_id}
            className={s.relatedChip}
            style={{ ["--i" as string]: i }}
            onClick={() => router.push(`/pages/${r.page_id}`)}
          >
            {r.title}
          </button>
        ))}
      </div>
    </aside>
  );
}
