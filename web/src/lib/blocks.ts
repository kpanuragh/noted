import { Node, mergeAttributes } from "@tiptap/core";

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
