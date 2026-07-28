"use client";

import { Suspense, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useWorkspace } from "@/lib/useWorkspace";
import { useSearchParams } from "next/navigation";
import { Sidebar } from "@/components/Sidebar";
import { api, type SearchHit } from "@/lib/api";
import s from "@/components/ui.module.css";

const DEBOUNCE_MS = 200;

/**
 * The full-page hybrid search surface: full-text + semantic search over page
 * content, as opposed to Cmd+K's QuickFind, which is lexical and title-only.
 * These are deliberately separate surfaces — QuickFind is navigational,
 * this is content search — see QuickFind.tsx for the split rationale.
 */
function SearchPage() {
  const searchParams = useSearchParams();
  const ws = useWorkspace();
  const workspaceId = ws.status === "ready" ? ws.current : "";
  const [q, setQ] = useState(() => searchParams.get("q") ?? "");
  const [results, setResults] = useState<SearchHit[]>([]);
  const [searched, setSearched] = useState(false);
  const [busy, setBusy] = useState(false);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => {
    if (debounceRef.current) clearTimeout(debounceRef.current);
    // Wait for the workspace to resolve; searching "" would 400.
    if (!workspaceId) return;
    if (!q.trim()) {
      setResults([]);
      setSearched(false);
      setBusy(false);
      return;
    }
    setBusy(true);
    debounceRef.current = setTimeout(() => {
      api
        .search(workspaceId, q)
        .then((hits) => {
          setResults(hits);
          setSearched(true);
        })
        .catch(console.error)
        .finally(() => setBusy(false));
    }, DEBOUNCE_MS);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [q, workspaceId]);

  return (
    <div className={s.app}>
      <Sidebar workspaceId={workspaceId} />
      <main className={s.main} style={{ maxWidth: 780 }}>
        <header className={s.enter} style={{ marginBottom: 22 }}>
          <Link href="/" className={s.backLink}>
            ← All notes
          </Link>
          <h1 style={{ marginBottom: 8 }}>Search</h1>
          <p className={s.lede}>
            Searches what your notes say, not just their titles — so a note can
            match on meaning even when it never uses your exact words.
          </p>
        </header>

        <input
          autoFocus
          className={`${s.field} ${s.enter}`}
          style={{ ["--i" as string]: 1, fontSize: "1rem", padding: "11px 14px" }}
          placeholder="Search your notes…"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          aria-label="Search your notes"
        />

        <div style={{ marginTop: 20 }}>
          {busy && (
            <div aria-hidden="true">
              {[68, 84, 55].map((w, i) => (
                <div key={i} className={s.skeletonRow}>
                  <div className={s.skeleton} style={{ width: "28%" }} />
                  <div className={s.skeleton} style={{ width: `${w}%` }} />
                </div>
              ))}
            </div>
          )}

          {!busy && q.trim() && searched && results.length === 0 && (
            // An empty result is a dead end unless it says what to do next.
            <p className={s.empty}>
              Nothing matches “{q.trim()}”. Try a word you would have written in
              the note itself — search reads your notes' content, not their
              titles.
            </p>
          )}

          {!busy && !q.trim() && (
            <p className={s.empty}>
              Type to search across everything you have written.
            </p>
          )}

          {!busy && results.length > 0 && (
            <>
              <p className={s.eyebrow} style={{ marginBottom: 6 }}>
                {results.length} result{results.length === 1 ? "" : "s"}
              </p>
              <div className={s.list}>
                {results.map((hit, i) => (
                  <Link
                    key={hit.page_id}
                    href={`/pages/${hit.page_id}`}
                    className={s.searchRow}
                    style={{ ["--i" as string]: i }}
                  >
                    <span className={s.rowTitle}>{hit.title}</span>
                    <p className={s.searchSnippet}>{hit.snippet}</p>
                  </Link>
                ))}
              </div>
            </>
          )}
        </div>
      </main>
    </div>
  );
}

export default function Search() {
  return (
    <Suspense fallback={null}>
      <SearchPage />
    </Suspense>
  );
}
