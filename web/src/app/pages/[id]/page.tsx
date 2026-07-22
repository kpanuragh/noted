"use client";

import { use, useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { Editor } from "@/components/Editor";
import { PageTree } from "@/components/PageTree";
import { RelatedNotes } from "@/components/RelatedNotes";
import { api, type Page } from "@/lib/api";
import { PageTitle } from "@/components/PageTitle";
import { useWorkspace } from "@/lib/useWorkspace";



export default function PageView({ params }: { params: Promise<{ id: string }> }) {
  const { id } = use(params);
  const router = useRouter();
  const [page, setPage] = useState<Page | null>(null);
  const ws = useWorkspace();
  const workspaceId = ws.status === "ready" ? ws.current : "";

  useEffect(() => {
    setPage(null);
    api.getPage(id).then(setPage).catch(console.error);
  }, [id]);

  return (
    <main style={{ display: "flex", gap: 24, padding: 24 }}>
      <nav style={{ minWidth: 220 }}>
        <PageTree workspaceId={workspaceId} onSelect={(p) => router.push(`/pages/${p.id}`)} />
      </nav>
      <section style={{ flex: 1 }}>
        {page ? (
          <PageTitle
            pageId={id}
            initial={page.title}
            onRenamed={(title) => setPage({ ...page, title })}
          />
        ) : (
          <h1>Loading…</h1>
        )}
        <Editor pageId={id} />
      </section>
      <RelatedNotes pageId={id} />
    </main>
  );
}
