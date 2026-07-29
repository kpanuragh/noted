"use client";

import { useEffect, useMemo, useState } from "react";
import { EditorContent, useEditor, type Editor as TiptapEditor } from "@tiptap/react";
import { BubbleMenu } from "@tiptap/react/menus";
import { shouldShowTableMenu, shouldShowTextMenu } from "@/lib/bubbleMenus";
import Collaboration from "@tiptap/extension-collaboration";
import * as Y from "yjs";
import { createProvider } from "@/lib/provider";
import { editorExtensions } from "@/lib/editorExtensions";
import { isSafeHref } from "@/lib/editorExtensions";
import s from "./editor.module.css";

/**
 * A formatting control in the selection menu.
 *
 * `active` is read from the editor rather than tracked separately, so the menu
 * can never disagree with the document about whether the selection is bold.
 */
function Tool({
  label,
  title,
  active,
  danger,
  glyph,
  onClick,
}: {
  label: React.ReactNode;
  title: string;
  active?: boolean;
  danger?: boolean;
  glyph?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      title={title}
      aria-label={title}
      aria-pressed={active ?? false}
      className={danger ? s.toolDanger : active ? s.toolActive : glyph ? s.toolGlyph : s.tool}
      // Prevented so pressing a control does not collapse the selection it is
      // about to act on — the whole menu depends on the selection surviving.
      onMouseDown={(e) => e.preventDefault()}
      onClick={onClick}
    >
      {label}
    </button>
  );
}

export function Editor({ pageId }: { pageId: string }) {
  const doc = useMemo(() => new Y.Doc(), [pageId]);
  const [editing, setEditing] = useState(false);
  const [wrap, setWrap] = useState<HTMLElement | null>(null);

  useEffect(() => {
    const provider = createProvider(pageId, doc);
    return () => provider.destroy();
  }, [pageId, doc]);

  const editor = useEditor(
    {
      immediatelyRender: false,
      extensions: [
        ...editorExtensions(),
        // `field` must match noted_crdt::ROOT on the server.
        Collaboration.configure({ document: doc, field: "prosemirror" }),
      ],
      onFocus: () => setEditing(true),
      onBlur: () => setEditing(false),
    },
    [doc],
  );

  function setLink(ed: TiptapEditor) {
    const current = (ed.getAttributes("link").href as string | undefined) ?? "";
    const url = window.prompt("Link URL", current);
    if (url === null) return; // cancelled
    if (url.trim() === "") {
      ed.chain().focus().extendMarkRange("link").unsetLink().run();
      return;
    }
    // The same allowlist the paste sanitiser uses — a link typed here is no
    // more trustworthy than one pasted, and both end up rendered and clickable.
    if (!isSafeHref(url)) {
      window.alert("That link can't be used. Use an http, https or mailto address.");
      return;
    }
    ed.chain().focus().extendMarkRange("link").setLink({ href: url.trim() }).run();
  }

  // Both menus are kept inside the document's own column. Left to the viewport,
  // a menu anchored to a cell in the first column of a table extends several
  // hundred pixels to the left and lands on top of the sidebar, covering the
  // navigation. A callback ref rather than useRef: this has to trigger a render
  // once the element exists, or the menus mount with no boundary at all.
  const menuOptions = wrap
    ? { shift: { boundary: wrap, padding: 8 }, flip: { boundary: wrap } }
    : undefined;

  return (
    <div
      ref={setWrap}
      className={editing ? `${s.wrap} ${s.wrapEditing}` : s.wrap}
    >
      {/*
        Formatting appears AT the selection rather than in a permanent bar.
        A note is mostly read, and a toolbar pinned above every document is
        chrome you pay for on every glance to use occasionally. Block structure
        still comes from markdown as you type (# for a heading, - for a list),
        which the empty-document placeholder states.
      */}
      {editor && (
        <BubbleMenu
          editor={editor}
          options={menuOptions}
          className={s.bubble}
          // Two bubble menus anchor to the same selection, so they must never
          // both qualify or they render on top of each other. Text formatting
          // yields whenever the table menu claims the selection.
          shouldShow={({ state }) => shouldShowTextMenu(state.selection)}
        >
          <Tool
            label={<strong>B</strong>}
            title="Bold"
            active={editor.isActive("bold")}
            onClick={() => editor.chain().focus().toggleBold().run()}
          />
          <Tool
            label={<em>I</em>}
            title="Italic"
            active={editor.isActive("italic")}
            onClick={() => editor.chain().focus().toggleItalic().run()}
          />
          <Tool
            label={<s>S</s>}
            title="Strikethrough"
            active={editor.isActive("strike")}
            onClick={() => editor.chain().focus().toggleStrike().run()}
          />
          <Tool
            label="&lt;/&gt;"
            title="Inline code"
            active={editor.isActive("code")}
            onClick={() => editor.chain().focus().toggleCode().run()}
          />
          <Tool
            label="🔗"
            title={editor.isActive("link") ? "Edit link" : "Add link"}
            active={editor.isActive("link")}
            onClick={() => setLink(editor)}
          />

          <span className={s.toolSep} />

          <Tool
            label="H1"
            title="Heading 1"
            active={editor.isActive("heading", { level: 1 })}
            onClick={() => editor.chain().focus().toggleHeading({ level: 1 }).run()}
          />
          <Tool
            label="H2"
            title="Heading 2"
            active={editor.isActive("heading", { level: 2 })}
            onClick={() => editor.chain().focus().toggleHeading({ level: 2 }).run()}
          />
          <Tool
            label="H3"
            title="Heading 3"
            active={editor.isActive("heading", { level: 3 })}
            onClick={() => editor.chain().focus().toggleHeading({ level: 3 }).run()}
          />

          <span className={s.toolSep} />

          <Tool
            label="•"
            title="Bulleted list"
            active={editor.isActive("bulletList")}
            onClick={() => editor.chain().focus().toggleBulletList().run()}
          />
          <Tool
            label="1."
            title="Numbered list"
            active={editor.isActive("orderedList")}
            onClick={() => editor.chain().focus().toggleOrderedList().run()}
          />
          <Tool
            label="❝"
            title="Quote"
            active={editor.isActive("blockquote")}
            onClick={() => editor.chain().focus().toggleBlockquote().run()}
          />
        </BubbleMenu>
      )}

      {/*
        Table controls, on their own menu.
        A table needs acting on when the caret is merely INSIDE it — you add a
        row without selecting anything — so this cannot ride on the selection
        menu, which by definition appears only when text is selected. Hence a
        second menu with its own `shouldShow` and a distinct `pluginKey`; two
        bubble menus sharing a key would fight over the same plugin slot.
      */}
      {editor && (
        <BubbleMenu
          editor={editor}
          pluginKey="tableMenu"
          options={menuOptions}
          className={s.bubbleWide}
          // Inside a table with nothing selected — you add a row without
          // selecting anything — or with whole cells selected, which is the
          // only time merging means anything. Selecting TEXT in a cell is a
          // formatting intent, and that case belongs to the menu above.
          shouldShow={({ editor: ed, state }) => shouldShowTableMenu(ed.isActive("table"), state.selection)}
        >
          <span className={s.menuLabel}>Row</span>
          <Tool glyph label="↑+" title="Insert row above" onClick={() => editor.chain().focus().addRowBefore().run()} />
          <Tool glyph label="↓+" title="Insert row below" onClick={() => editor.chain().focus().addRowAfter().run()} />
          <Tool glyph label="↕✕" title="Delete row" onClick={() => editor.chain().focus().deleteRow().run()} />

          <span className={s.toolSep} />

          <span className={s.menuLabel}>Col</span>
          <Tool glyph label="←+" title="Insert column left" onClick={() => editor.chain().focus().addColumnBefore().run()} />
          <Tool glyph label="→+" title="Insert column right" onClick={() => editor.chain().focus().addColumnAfter().run()} />
          <Tool glyph label="↔✕" title="Delete column" onClick={() => editor.chain().focus().deleteColumn().run()} />

          <span className={s.toolSep} />

          <Tool
            label="Header"
            title="Toggle header row"
            active={editor.isActive("tableHeader")}
            onClick={() => editor.chain().focus().toggleHeaderRow().run()}
          />
          <Tool label="Merge" title="Merge or split cells" onClick={() => editor.chain().focus().mergeOrSplit().run()} />

          <span className={s.toolSep} />

          {/* Deleting the whole table is the one destructive action here, so it
              is set apart and coloured rather than sitting inline with the
              row and column controls a writer uses constantly. */}
          <Tool
            label="Delete table"
            title="Delete table"
            danger
            onClick={() => editor.chain().focus().deleteTable().run()}
          />
        </BubbleMenu>
      )}

      <div className={s.surface}>
        <EditorContent editor={editor} />
      </div>
    </div>
  );
}
