"use client";

import { useRouter } from "next/navigation";
import { PageTree } from "@/components/PageTree";
import { api } from "@/lib/api";

const WORKSPACE_ID = process.env.NEXT_PUBLIC_WORKSPACE_ID ?? "";

export default function Home() {
  const router = useRouter();

  async function handleNewPage() {
    const page = await api.createPage(WORKSPACE_ID, null, "Untitled");
    router.push(`/pages/${page.id}`);
  }

  return (
    <main style={{ display: "flex", gap: 24, padding: 24 }}>
      <nav style={{ minWidth: 220 }}>
        <button onClick={handleNewPage}>New page</button>
        <PageTree workspaceId={WORKSPACE_ID} onSelect={(p) => router.push(`/pages/${p.id}`)} />
      </nav>
      <section>
        <p>Select a page</p>
      </section>
    </main>
  );
}
