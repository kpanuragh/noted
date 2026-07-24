"use client";

import { useEffect, useState } from "react";
import { QuickFind } from "@/components/QuickFind";
import { useWorkspace } from "@/lib/useWorkspace";

/**
 * Owns the Cmd+K / Ctrl+K quick-find state for the whole app and mounts
 * QuickFind in place of the rest of the tree — see QuickFind.tsx for why
 * that's a swap, not an overlay stacked on top.
 *
 * The workspace comes from `useWorkspace`, NOT `NEXT_PUBLIC_WORKSPACE_ID`. It
 * used to read that build-time env var, so every signed-in user's quick-find
 * searched whatever workspace was baked at build — the seeded default, or an
 * empty string that the API rejected with a 400. A workspace belongs to whoever
 * is a member of it; quick-find has to ask the session, the same way the
 * dashboard already does.
 */
export function AppShell({ children }: { children: React.ReactNode }) {
  const [open, setOpen] = useState(false);
  const ws = useWorkspace();
  const workspaceId = ws.status === "ready" ? ws.current : "";

  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOpen(true);
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  // Only swap in quick-find once a workspace is known. Opening it without one
  // would query `workspace_id=` and get a 400 — the bug this file used to have.
  if (open && workspaceId) {
    return <QuickFind workspaceId={workspaceId} onClose={() => setOpen(false)} />;
  }
  return <>{children}</>;
}
