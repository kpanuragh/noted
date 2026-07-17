"use client";

import { use, useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { Editor } from "@/components/Editor";
import { PageTree } from "@/components/PageTree";
import { RelatedNotes } from "@/components/RelatedNotes";
import { api, type Page } from "@/lib/api";

const WORKSPACE_ID = process.env.NEXT_PUBLIC_WORKSPACE_ID ?? "";

export default function PageView({ params }: { params: Promise<{ id: string }> }) {
  const { id } = use(params);
  const router = useRouter();
  const [page, setPage] = useState<Page | null>(null);

  useEffect(() => {
    setPage(null);
    api.getPage(id).then(setPage).catch(console.error);
  }, [id]);

  return (
    <main style={{ display: "flex", gap: 24, padding: 24 }}>
      <nav style={{ minWidth: 220 }}>
        <PageTree workspaceId={WORKSPACE_ID} onSelect={(p) => router.push(`/pages/${p.id}`)} />
      </nav>
      <section style={{ flex: 1 }}>
        <h1>{page ? page.title : "Loading…"}</h1>
        <Editor pageId={id} />
      </section>
      <RelatedNotes pageId={id} />
    </main>
  );
}
