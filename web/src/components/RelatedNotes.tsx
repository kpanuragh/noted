"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { api, type RelatedHit } from "@/lib/api";

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
    <aside style={{ minWidth: 200 }}>
      <h2>Related</h2>
      <ul>
        {related.map((r) => (
          <li key={r.page_id}>
            <button onClick={() => router.push(`/pages/${r.page_id}`)}>{r.title}</button>
          </li>
        ))}
      </ul>
    </aside>
  );
}
