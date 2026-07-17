"use client";

import { use } from "react";
import { Editor } from "@/components/Editor";

export default function PageView({ params }: { params: Promise<{ id: string }> }) {
  const { id } = use(params);
  return (
    <main style={{ padding: 24 }}>
      <Editor pageId={id} />
    </main>
  );
}
