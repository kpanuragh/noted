"use client";

import { Sidebar } from "@/components/Sidebar";
import { RecentPages } from "@/components/RecentPages";
import { WorkspaceStatsPanel } from "@/components/WorkspaceStatsPanel";
import { PanelBoundary } from "@/components/PanelBoundary";
import { useWorkspace } from "@/lib/useWorkspace";
import s from "@/components/ui.module.css";

export default function Home() {
  const ws = useWorkspace();
  const workspaceId = ws.status === "ready" ? ws.current : "";

  return (
    <div className={s.app}>
      {/* New page / Search / Ask now live in the sidebar, where they are one
          click away from every screen — so the dashboard body no longer repeats
          them and can be just the heading and the panels. */}
      <Sidebar workspaceId={workspaceId} />

      <main className={s.main}>
        <header className={s.enter} style={{ marginBottom: 28 }}>
          <h1 style={{ marginBottom: 8 }}>Your workspace</h1>
          <p className={s.lede}>
            Pick up where you left off, or ask your notes a question.
          </p>
        </header>

        <div className={s.panels}>
          <div className={s.enter} style={{ ["--i" as string]: 1 }}>
          <PanelBoundary title="Recently edited">
            {workspaceId && <RecentPages workspaceId={workspaceId} />}
          </PanelBoundary>
          </div>
          <div className={s.enter} style={{ ["--i" as string]: 2 }}>
          <PanelBoundary title="Your knowledge base">
            {workspaceId && <WorkspaceStatsPanel workspaceId={workspaceId} />}
          </PanelBoundary>
          </div>
        </div>
      </main>
    </div>
  );
}
