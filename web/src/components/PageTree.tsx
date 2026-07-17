"use client";

import { useEffect, useState } from "react";
import { api, type Page } from "@/lib/api";

export function PageTree({
  workspaceId,
  parentId,
  onSelect,
}: {
  workspaceId: string;
  parentId?: string;
  onSelect: (page: Page) => void;
}) {
  const [pages, setPages] = useState<Page[]>([]);

  useEffect(() => {
    api.listPages(workspaceId, parentId).then(setPages).catch(console.error);
  }, [workspaceId, parentId]);

  return (
    <ul>
      {pages.map((p) => (
        <li key={p.id}>
          <button onClick={() => onSelect(p)}>{p.title}</button>
          <PageTree workspaceId={workspaceId} parentId={p.id} onSelect={onSelect} />
        </li>
      ))}
    </ul>
  );
}
