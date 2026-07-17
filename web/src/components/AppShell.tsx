"use client";

import { useEffect, useState } from "react";
import { QuickFind } from "@/components/QuickFind";

const WORKSPACE_ID = process.env.NEXT_PUBLIC_WORKSPACE_ID ?? "";

/**
 * Owns the Cmd+K / Ctrl+K quick-find state for the whole app and mounts
 * QuickFind in place of the rest of the tree — see QuickFind.tsx for why
 * that's a swap, not an overlay stacked on top.
 */
export function AppShell({ children }: { children: React.ReactNode }) {
  const [open, setOpen] = useState(false);

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

  if (open) {
    return <QuickFind workspaceId={WORKSPACE_ID} onClose={() => setOpen(false)} />;
  }
  return <>{children}</>;
}
