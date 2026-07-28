import StarterKit from "@tiptap/starter-kit";
import Link from "@tiptap/extension-link";
import Image from "@tiptap/extension-image";
import { TableKit } from "@tiptap/extension-table";
import { TaskList, TaskItem } from "@tiptap/extension-list";
import Youtube from "@tiptap/extension-youtube";
import { Callout, ToggleBlock, ToggleSummary, ToggleBody } from "@/lib/blocks";
import { SlashCommand } from "@/lib/slashCommand";

/**
 * The URL schemes a link may use.
 *
 * An allowlist rather than a denylist. `javascript:` is the obvious one to
 * exclude, but `data:` and `vbscript:` are equally executable, and a denylist
 * only ever excludes the schemes someone thought of. These links are rendered
 * and clickable, and their href arrives from whatever the user pasted, so this
 * is a real injection surface rather than a theoretical one.
 */
export const ALLOWED_PROTOCOLS = ["http", "https", "mailto"] as const;

export function isSafeHref(href: string | null | undefined): boolean {
  if (!href) return false;
  const trimmed = href.trim();
  // Relative and anchor links carry no scheme and cannot execute.
  if (trimmed.startsWith("/") || trimmed.startsWith("#")) return true;
  try {
    const scheme = new URL(trimmed).protocol.replace(":", "").toLowerCase();
    return (ALLOWED_PROTOCOLS as readonly string[]).includes(scheme);
  } catch {
    // Not parseable as an absolute URL and not obviously relative. Reject:
    // a link nobody can resolve is worth less than the risk of guessing.
    return false;
  }
}

/**
 * Link, reduced to the one attribute that carries meaning.
 *
 * Tiptap's Link declares `href`, `target`, `rel` AND `class`, and parses all
 * four off a pasted `<a>`. Pasting a Wikipedia article therefore stored marks
 * like:
 *
 *     <link href="https://en.wikipedia.org/wiki/Reptile" class="null"
 *           rel="mw:WikiLink" target="_blank" title="Reptile">reptiles</link>
 *
 * None of `class`, `rel`, `title` or the source's `target` describes the user's
 * document — they describe the page it was copied from. They are someone else's
 * markup riding along in our content.
 *
 * `target` and `rel` are still RENDERED, but as our decision in
 * `HTMLAttributes` below rather than as parsed data. That is the important
 * distinction: they are presentation applied at render time, not content stored
 * in the document, so they cannot vary with wherever a paste came from.
 */
export const CleanLink = Link.extend({
  addAttributes() {
    return {
      href: {
        default: null,
        parseHTML: (element) => {
          const href = element.getAttribute("href");
          return isSafeHref(href) ? href : null;
        },
        renderHTML: (attributes) =>
          attributes.href ? { href: attributes.href } : {},
      },
    };
  },
});

/**
 * The editor's extension set.
 *
 * Exported separately from the React component so the schema can be asserted
 * without mounting an editor — the paste behaviour under test is a property of
 * the schema, and a test that has to render React to reach it would be testing
 * the wrong layer.
 */
export function editorExtensions() {
  return [
    // Collaboration owns history; StarterKit's own must be disabled or
    // undo/redo fights the CRDT.
    //
    // `link: false` because StarterKit 3.x bundles Tiptap's Link, and having
    // both registered means two marks named `link` — the bundled one would
    // win and keep parsing the attributes CleanLink exists to drop.
    StarterKit.configure({ undoRedo: false, link: false }),
    // Type "/" at the start of a line to insert a block.
    SlashCommand,
    // Blocks beyond StarterKit's prose set. Each one had to survive the CRDT
    // projection before it could be offered: a block the writer can insert but
    // the index cannot read is worse than one that does not exist.
    TableKit.configure({ table: { resizable: true } }),
    TaskList,
    TaskItem.configure({ nested: true }),
    Image.configure({ inline: false }),
    Youtube.configure({ controls: true, nocookie: true }),
    // Ours — see blocks.ts. Tiptap's toggle is behind a paid plan.
    Callout,
    ToggleBlock,
    ToggleSummary,
    ToggleBody,
    CleanLink.configure({
      openOnClick: false,
      autolink: true,
      protocols: [...ALLOWED_PROTOCOLS],
      HTMLAttributes: {
        // Ours, not the source document's. `noopener` is the one that matters:
        // without it a link opened in a new tab can reach back through
        // `window.opener`.
        target: "_blank",
        rel: "noopener noreferrer nofollow",
      },
    }),
  ];
}
