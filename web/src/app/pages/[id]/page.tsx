"use client";

import { use, useEffect, useState } from "react";
import Link from "next/link";
import { Editor } from "@/components/Editor";
import { Sidebar } from "@/components/Sidebar";
import { RelatedNotes } from "@/components/RelatedNotes";
import { api, type Page } from "@/lib/api";
import s from "@/components/ui.module.css";
import { PageTitle } from "@/components/PageTitle";
import { useWorkspace } from "@/lib/useWorkspace";



export default function PageView({ params }: { params: Promise<{ id: string }> }) {
  const { id } = use(params);
  const [page, setPage] = useState<Page | null>(null);
  const [treeVersion, setTreeVersion] = useState(0);
  const ws = useWorkspace();
  const workspaceId = ws.status === "ready" ? ws.current : "";

  useEffect(() => {
    setPage(null);
    api.getPage(id).then(setPage).catch(console.error);
  }, [id]);

  return (
    <main className={s.app}>
      <Sidebar workspaceId={workspaceId} refreshKey={treeVersion} />
      <section className={s.main} style={{ maxWidth: 760 }}>
        <Link href="/" className={s.backLink}>
          ← All notes
        </Link>
        {page ? (
          <PageTitle
            pageId={id}
            initial={page.title}
            onRenamed={(title) => {
              setPage({ ...page, title });
              // The sidebar is showing the old title; tell it to refetch.
              setTreeVersion((v) => v + 1);
            }}
          />
        ) : (
          <h1>Loading…</h1>
        )}
        <Editor pageId={id} />
        <RelatedNotes pageId={id} />
      </section>
    </main>
  );
}
