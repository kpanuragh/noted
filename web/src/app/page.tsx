"use client";

import { useState } from "react";
import { PageTree } from "@/components/PageTree";
import type { Page } from "@/lib/api";

const WORKSPACE_ID = process.env.NEXT_PUBLIC_WORKSPACE_ID ?? "";

export default function Home() {
  const [selected, setSelected] = useState<Page | null>(null);
  return (
    <main style={{ display: "flex", gap: 24, padding: 24 }}>
      <nav style={{ minWidth: 220 }}>
        <PageTree workspaceId={WORKSPACE_ID} onSelect={setSelected} />
      </nav>
      <section>{selected ? <h1>{selected.title}</h1> : <p>Select a page</p>}</section>
    </main>
  );
}
