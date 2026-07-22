"use client";

import { Suspense, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useWorkspace } from "@/lib/useWorkspace";
import { useSearchParams } from "next/navigation";
import { api, type SearchHit } from "@/lib/api";


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
  const debounceRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => {
    if (debounceRef.current) clearTimeout(debounceRef.current);
    // Wait for the workspace to resolve; searching "" would 400.
    if (!workspaceId) return;
    if (!q.trim()) {
      setResults([]);
      setSearched(false);
      return;
    }
    debounceRef.current = setTimeout(() => {
      api
        .search(workspaceId, q)
        .then((hits) => {
          setResults(hits);
          setSearched(true);
        })
        .catch(console.error);
    }, DEBOUNCE_MS);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [q, workspaceId]);

  return (
    <main style={{ padding: 24 }}>
      <h1>Search</h1>
      <input
        autoFocus
        placeholder="Search page content…"
        value={q}
        onChange={(e) => setQ(e.target.value)}
        style={{ width: "min(560px, 90vw)", fontSize: 16, padding: 8 }}
      />
      {q.trim() && searched && results.length === 0 && <p>No results</p>}
      <ul style={{ listStyle: "none", margin: "16px 0 0", padding: 0 }}>
        {results.map((hit) => (
          <li key={hit.page_id} style={{ marginBottom: 16 }}>
            <Link href={`/pages/${hit.page_id}`}>{hit.title}</Link>
            <p>{hit.snippet}</p>
          </li>
        ))}
      </ul>
    </main>
  );
}

export default function Search() {
  return (
    <Suspense fallback={null}>
      <SearchPage />
    </Suspense>
  );
}
