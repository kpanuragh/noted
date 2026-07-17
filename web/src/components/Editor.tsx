"use client";

import { useEffect, useMemo } from "react";
import { EditorContent, useEditor } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import Collaboration from "@tiptap/extension-collaboration";
import * as Y from "yjs";
import { createProvider } from "@/lib/provider";

export function Editor({ pageId }: { pageId: string }) {
  const doc = useMemo(() => new Y.Doc(), [pageId]);

  useEffect(() => {
    const provider = createProvider(pageId, doc);
    return () => provider.destroy();
  }, [pageId, doc]);

  const editor = useEditor(
    {
      immediatelyRender: false,
      extensions: [
        // Collaboration owns history; StarterKit's own must be disabled or
        // undo/redo fights the CRDT.
        StarterKit.configure({ undoRedo: false }),
        // `field` must match noted_crdt::ROOT on the server.
        Collaboration.configure({ document: doc, field: "prosemirror" }),
      ],
    },
    [doc],
  );

  return <EditorContent editor={editor} />;
}
