"use client";

import { use, useEffect, useState } from "react";
import Link from "next/link";
import { Editor } from "@/components/Editor";
import { Sidebar } from "@/components/Sidebar";
import { RelatedNotes } from "@/components/RelatedNotes";
import { api, NotFoundError, type Page } from "@/lib/api";
import s from "@/components/ui.module.css";
import { PageTitle } from "@/components/PageTitle";
import { useWorkspace } from "@/lib/useWorkspace";



export default function PageView({ params }: { params: Promise<{ id: string }> }) {
  const { id } = use(params);
  // Four states, not two. `page === null` used to mean both "still loading"
  // and "the request failed", so a note that had been deleted — or any failed
  // fetch — sat on "Loading…" forever with an editor underneath it, inviting
  // you to type into a note that does not exist.
  const [page, setPage] = useState<Page | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "missing" | "failed">("loading");
  const [treeVersion, setTreeVersion] = useState(0);
  const ws = useWorkspace();
  const workspaceId = ws.status === "ready" ? ws.current : "";

  useEffect(() => {
    let cancelled = false;
    setPage(null);
    setStatus("loading");
    api
      .getPage(id)
      .then((p) => {
        if (cancelled) return;
        setPage(p);
        setStatus("ready");
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        setStatus(err instanceof NotFoundError ? "missing" : "failed");
      });
    return () => {
      cancelled = true;
    };
  }, [id]);

  return (
    <main className={s.app}>
      <Sidebar workspaceId={workspaceId} refreshKey={treeVersion} />
      <section className={s.main} style={{ maxWidth: 760 }}>
        <Link href="/" className={s.backLink}>
          ← All notes
        </Link>
        {status === "ready" && page && (
          <PageTitle
            pageId={id}
            initial={page.title}
            onRenamed={(title) => {
              setPage({ ...page, title });
              // The sidebar is showing the old title; tell it to refetch.
              setTreeVersion((v) => v + 1);
            }}
          />
        )}
        {status === "loading" && <h1>Loading…</h1>}

        {/* A note that is gone says so, and offers the way out. Rendering the
            editor here would be worse than the old permanent "Loading…": it
            would take keystrokes for a note with nowhere to save them. */}
        {status === "missing" && (
          <>
            <h1>This note no longer exists</h1>
            <p className={s.lede} style={{ marginTop: 8 }}>
              It may have been deleted. Nothing you have written elsewhere is
              affected.
            </p>
            <p style={{ marginTop: 16 }}>
              <Link href="/" className={s.buttonQuiet} style={{ textDecoration: "none" }}>
                Back to all notes
              </Link>
            </p>
          </>
        )}

        {status === "failed" && (
          <>
            <h1>Couldn&apos;t open this note</h1>
            <p className={s.lede} style={{ marginTop: 8 }}>
              The note is safe — this was a problem reaching the server.
            </p>
            <p style={{ marginTop: 16 }}>
              <button className={s.buttonQuiet} onClick={() => location.reload()}>
                Try again
              </button>
            </p>
          </>
        )}

        {status === "ready" && (
          <>
            <Editor pageId={id} />
            <RelatedNotes pageId={id} />
          </>
        )}
      </section>
    </main>
  );
}
