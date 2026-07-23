"use client";

import { useEffect, useMemo } from "react";
import { EditorContent, useEditor, type Editor as TiptapEditor } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import Collaboration from "@tiptap/extension-collaboration";
import * as Y from "yjs";
import { createProvider } from "@/lib/provider";
import s from "./editor.module.css";

/**
 * A formatting control.
 *
 * `isActive` is read from the editor rather than tracked separately, so the
 * toolbar can never disagree with the document about whether the cursor is in a
 * heading — a toolbar that lies is worse than no toolbar.
 */
function Tool({
  editor,
  label,
  title,
  active,
  onClick,
}: {
  editor: TiptapEditor;
  label: string;
  title: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      title={title}
      aria-label={title}
      aria-pressed={active}
      className={active ? s.toolActive : s.tool}
      // `onMouseDown` prevented: clicking a toolbar button must not steal focus
      // from the document, or the command applies to a collapsed selection
      // somewhere the user is not looking.
      onMouseDown={(e) => e.preventDefault()}
      onClick={onClick}
      disabled={!editor.isEditable}
    >
      {label}
    </button>
  );
}

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

  return (
    <div className={s.wrap}>
      {editor && (
        <div className={s.toolbar} role="toolbar" aria-label="Formatting">
          <Tool
            editor={editor}
            label="B"
            title="Bold"
            active={editor.isActive("bold")}
            onClick={() => editor.chain().focus().toggleBold().run()}
          />
          <Tool
            editor={editor}
            label="I"
            title="Italic"
            active={editor.isActive("italic")}
            onClick={() => editor.chain().focus().toggleItalic().run()}
          />
          <Tool
            editor={editor}
            label="Code"
            title="Inline code"
            active={editor.isActive("code")}
            onClick={() => editor.chain().focus().toggleCode().run()}
          />
          <span className={s.toolSep} />
          <Tool
            editor={editor}
            label="H1"
            title="Heading 1"
            active={editor.isActive("heading", { level: 1 })}
            onClick={() => editor.chain().focus().toggleHeading({ level: 1 }).run()}
          />
          <Tool
            editor={editor}
            label="H2"
            title="Heading 2"
            active={editor.isActive("heading", { level: 2 })}
            onClick={() => editor.chain().focus().toggleHeading({ level: 2 }).run()}
          />
          <span className={s.toolSep} />
          <Tool
            editor={editor}
            label="List"
            title="Bulleted list"
            active={editor.isActive("bulletList")}
            onClick={() => editor.chain().focus().toggleBulletList().run()}
          />
          <Tool
            editor={editor}
            label="1."
            title="Numbered list"
            active={editor.isActive("orderedList")}
            onClick={() => editor.chain().focus().toggleOrderedList().run()}
          />
          <Tool
            editor={editor}
            label="Quote"
            title="Blockquote"
            active={editor.isActive("blockquote")}
            onClick={() => editor.chain().focus().toggleBlockquote().run()}
          />
          <span className={s.hint}>saves as you type</span>
        </div>
      )}
      <div className={s.surface}>
        <EditorContent editor={editor} />
      </div>
    </div>
  );
}
