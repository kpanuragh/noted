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
      {/* "Similar", not "Connected". These come from comparing what the notes
          MEAN — nearest embeddings — not from walking the knowledge graph, and
          calling them connections claimed a provenance the query does not have.
          In a product whose whole argument is showing how it reached an answer,
          mislabelling the method is the worst available bug.

          Amber still applies: the system derived this rather than the writer
          typing a link, which is exactly what the accent is reserved for. */}
      <p className={s.relatedTitle}>
        <span className={s.relatedGlyph}>◆</span> Similar notes
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
