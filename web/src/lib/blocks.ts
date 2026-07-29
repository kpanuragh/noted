import { Node, mergeAttributes } from "@tiptap/core";
import { TextSelection } from "@tiptap/pm/state";

/**
 * Blocks this project defines itself.
 *
 * Tiptap ships the prose set and sells the rest: its Details (toggle)
 * extension is behind a paid plan. These are written here rather than paid for
 * — they are small, and owning them means they can be shaped to this editor's
 * needs rather than configured around.
 *
 * Both render to real HTML elements with real semantics, which matters beyond
 * looks: the CRDT projection walks the document tree, so a node that nests its
 * text sensibly is a node search can read.
 */

declare module "@tiptap/core" {
  interface Commands<ReturnType> {
    callout: { setCallout: () => ReturnType; toggleCallout: () => ReturnType };
    toggleBlock: { setToggleBlock: () => ReturnType };
  }
}

/**
 * An aside worth noticing — Notion's callout.
 *
 * `content: "block+"` rather than "inline*": a callout holding a list or a
 * second paragraph is normal, and a node that only accepts inline content
 * silently drops what a writer pastes into it.
 */
export const Callout = Node.create({
  name: "callout",
  group: "block",
  content: "block+",
  defining: true,

  parseHTML() {
    return [{ tag: 'div[data-type="callout"]' }];
  },

  renderHTML({ HTMLAttributes }) {
    return ["div", mergeAttributes(HTMLAttributes, { "data-type": "callout" }), 0];
  },

  addCommands() {
    return {
      setCallout:
        () =>
        ({ commands }) =>
          commands.wrapIn(this.name),
      toggleCallout:
        () =>
        ({ commands }) =>
          commands.toggleWrap(this.name),
    };
  },
});

/**
 * A collapsible section — Notion's toggle.
 *
 * Rendered as native `<details>/<summary>`, so it opens and closes with no
 * JavaScript at all and stays keyboard-accessible for free. The alternative, a
 * NodeView with click handlers, would reimplement a browser primitive worse.
 *
 * Split into two child nodes rather than "first paragraph is the title": the
 * schema then guarantees a summary exists, instead of leaving the renderer to
 * cope with a toggle whose first child was deleted.
 */
export const ToggleSummary = Node.create({
  name: "toggleSummary",
  content: "inline*",
  parseHTML() {
    return [{ tag: "summary" }];
  },
  renderHTML({ HTMLAttributes }) {
    return ["summary", mergeAttributes(HTMLAttributes), 0];
  },
});

export const ToggleBody = Node.create({
  name: "toggleBody",
  content: "block+",
  parseHTML() {
    return [{ tag: 'div[data-type="toggle-body"]' }];
  },
  renderHTML({ HTMLAttributes }) {
    return ["div", mergeAttributes(HTMLAttributes, { "data-type": "toggle-body" }), 0];
  },
});

export const ToggleBlock = Node.create({
  name: "toggleBlock",
  group: "block",
  content: "toggleSummary toggleBody",
  defining: true,

  parseHTML() {
    return [{ tag: "details" }];
  },

  renderHTML({ HTMLAttributes }) {
    // `open` by default: a toggle that starts collapsed hides the content the
    // writer just typed into it.
    return ["details", mergeAttributes(HTMLAttributes, { open: "" }), 0];
  },

  addCommands() {
    return {
      setToggleBlock:
        () =>
        ({ chain }) =>
          chain()
            .insertContent({
              type: this.name,
              content: [
                { type: "toggleSummary", content: [{ type: "text", text: "Toggle" }] },
                { type: "toggleBody", content: [{ type: "paragraph" }] },
              ],
            })
            .run(),
    };
  },
});

declare module "@tiptap/core" {
  interface Commands<ReturnType> {
    columns: { setColumns: (count: number) => ReturnType };
  }
}

/**
 * One column of a side-by-side layout.
 *
 * `block+`, so a column can hold a heading and prose rather than a single
 * paragraph — a two-column layout whose halves cannot contain structure is a
 * table with extra steps.
 */
export const Column = Node.create({
  name: "column",
  content: "block+",
  isolating: true,

  parseHTML() {
    return [{ tag: 'div[data-type="column"]' }];
  },

  renderHTML({ HTMLAttributes }) {
    return ["div", mergeAttributes(HTMLAttributes, { "data-type": "column" }), 0];
  },
});

/**
 * Side-by-side columns.
 *
 * `isolating` on the child matters more than it looks: without it, backspace at
 * the start of a column merges it into the previous one and the layout quietly
 * collapses as you edit.
 *
 * The count lives in an attribute rather than in separate node types
 * (twoColumns, threeColumns…), so the CSS is one rule and changing a layout
 * later is an attribute update rather than a node replacement.
 */
export const Columns = Node.create({
  name: "columns",
  group: "block",
  content: "column{2,}",
  defining: true,

  addAttributes() {
    return {
      count: {
        default: 2,
        parseHTML: (el) => Number(el.getAttribute("data-count")) || 2,
        renderHTML: (attrs) => ({ "data-count": String(attrs.count) }),
      },
    };
  },

  parseHTML() {
    return [{ tag: 'div[data-type="columns"]' }];
  },

  renderHTML({ HTMLAttributes }) {
    return ["div", mergeAttributes(HTMLAttributes, { "data-type": "columns" }), 0];
  },

  addCommands() {
    return {
      setColumns:
        (count: number) =>
        ({ chain }) =>
          chain()
            .insertContent({
              type: this.name,
              attrs: { count },
              content: Array.from({ length: count }, () => ({
                type: "column",
                content: [{ type: "paragraph" }],
              })),
            })
            // `insertContent` leaves the caret after the whole node, which for
            // columns means the LAST one — so typing filled the right-hand
            // column and left the left one empty. Put it where a writer
            // expects to continue: the first column.
            .command(({ tr, dispatch }) => {
              if (!dispatch) return true;
              const { $from } = tr.selection;
              for (let depth = $from.depth; depth > 0; depth -= 1) {
                if ($from.node(depth).type.name !== this.name) continue;
                // columns start +1 into the first column, +1 into its
                // paragraph, +1 to sit inside that paragraph's text.
                const pos = $from.before(depth) + 3;
                dispatch(tr.setSelection(TextSelection.create(tr.doc, pos)));
                return true;
              }
              return true;
            })
            .run(),
    };
  },
});
