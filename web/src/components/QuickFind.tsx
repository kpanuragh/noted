"use client";

import { useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import { api, type QuickHit } from "@/lib/api";

const DEBOUNCE_MS = 150;

/**
 * The Cmd+K / Ctrl+K quick-find overlay. Controlled by its parent (see
 * AppShell): it is only ever mounted while open, and mounting it in place of
 * the rest of the app — rather than merely covering it — is deliberate. A
 * page's title can legitimately appear twice at once (once in the nav tree,
 * once in a quick-find result), and an overlay that only visually covers the
 * background leaves that duplicate in the DOM for anything that queries by
 * text or role. Swapping the tree instead of stacking on top of it keeps
 * "the page a user can see and click" and "the page in the accessibility
 * tree" the same thing.
 */
export function QuickFind({
  workspaceId,
  onClose,
}: {
  workspaceId: string;
  onClose: () => void;
}) {
  const router = useRouter();
  const [q, setQ] = useState("");
  const [results, setResults] = useState<QuickHit[]>([]);
  const inputRef = useRef<HTMLInputElement>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useEffect(() => {
    if (debounceRef.current) clearTimeout(debounceRef.current);
    if (!q.trim()) {
      setResults([]);
      return;
    }
    debounceRef.current = setTimeout(() => {
      api
        .quickFind(workspaceId, q)
        .then(setResults)
        .catch(console.error);
    }, DEBOUNCE_MS);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [q, workspaceId]);

  function navigateTo(pageId: string) {
    onClose();
    router.push(`/pages/${pageId}`);
  }

  return (
    <div
      role="dialog"
      aria-label="Quick find"
      aria-modal="true"
      onClick={onClose}
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0, 0, 0, 0.4)",
        display: "flex",
        alignItems: "flex-start",
        justifyContent: "center",
        paddingTop: "10vh",
        zIndex: 1000,
      }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        style={{
          background: "white",
          color: "black",
          borderRadius: 8,
          width: "min(560px, 90vw)",
          maxHeight: "70vh",
          padding: 12,
          boxShadow: "0 8px 30px rgba(0,0,0,0.3)",
          display: "flex",
          flexDirection: "column",
        }}
      >
        <input
          ref={inputRef}
          placeholder="Search pages…"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              onClose();
            } else if (e.key === "Enter" && results.length > 0) {
              navigateTo(results[0].page_id);
            }
          }}
          style={{ width: "100%", fontSize: 16, padding: 8, boxSizing: "border-box", flex: "none" }}
        />
        <ul
          style={{
            listStyle: "none",
            margin: "8px 0 0",
            padding: 0,
            overflowY: "auto",
          }}
        >
          {results.map((r) => (
            <li key={r.page_id}>
              <button
                onClick={() => navigateTo(r.page_id)}
                style={{
                  display: "block",
                  width: "100%",
                  textAlign: "left",
                  padding: "8px 6px",
                  background: "none",
                  border: "none",
                  cursor: "pointer",
                  font: "inherit",
                }}
              >
                {r.title}
              </button>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
