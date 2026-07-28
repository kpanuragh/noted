import { Extension } from "@tiptap/core";
import Suggestion from "@tiptap/suggestion";
import { ReactRenderer } from "@tiptap/react";
import {
  SlashMenu,
  filterSlashItems,
  type SlashItem,
  type SlashMenuHandle,
} from "@/components/SlashMenu";

/**
 * Type `/` to insert a block.
 *
 * Positioned by hand rather than with a popup library: the menu needs to sit at
 * the caret, flip above it near the bottom of the window, and follow the page
 * as it scrolls — which is a `getBoundingClientRect` and two comparisons, not a
 * dependency.
 */
export const SlashCommand = Extension.create({
  name: "slashCommand",

  addProseMirrorPlugins() {
    let renderer: ReactRenderer<SlashMenuHandle> | null = null;
    let container: HTMLDivElement | null = null;

    /** Place the menu at the caret, flipping up when it would fall off-screen. */
    const position = (rect: DOMRect | null) => {
      if (!container || !rect) return;
      const menu = container.firstElementChild as HTMLElement | null;
      const height = menu?.offsetHeight ?? 320;
      const width = menu?.offsetWidth ?? 280;
      const GAP = 6;

      const below = rect.bottom + GAP;
      const flip = below + height > window.innerHeight && rect.top - height - GAP > 0;

      container.style.top = `${flip ? rect.top - height - GAP : below}px`;
      // Keep it inside the window horizontally, so a caret near the right edge
      // does not push the menu off it.
      container.style.left = `${Math.min(rect.left, window.innerWidth - width - 8)}px`;
    };

    return [
      Suggestion<SlashItem>({
        editor: this.editor,
        char: "/",
        // Only at the start of an empty-ish line, as in Notion: a "/" typed
        // mid-sentence is punctuation, not a command.
        allowSpaces: false,
        startOfLine: true,
        items: ({ query }) => filterSlashItems(query),
        command: ({ editor, range, props }) => props.run(editor, range),
        render: () => ({
          onStart: (props) => {
            renderer = new ReactRenderer(SlashMenu, {
              props: {
                items: props.items,
                command: (item: SlashItem) => props.command(item),
              },
              editor: props.editor,
            });
            container = document.createElement("div");
            container.style.position = "fixed";
            container.style.zIndex = "1000";
            container.appendChild(renderer.element);
            document.body.appendChild(container);
            position(props.clientRect?.() ?? null);
          },
          onUpdate: (props) => {
            renderer?.updateProps({
              items: props.items,
              command: (item: SlashItem) => props.command(item),
            });
            position(props.clientRect?.() ?? null);
          },
          onKeyDown: (props) => {
            if (props.event.key === "Escape") {
              // Let the plugin close itself; onExit does the teardown.
              return false;
            }
            return renderer?.ref?.onKeyDown(props.event) ?? false;
          },
          onExit: () => {
            container?.remove();
            renderer?.destroy();
            container = null;
            renderer = null;
          },
        }),
      }),
    ];
  },
});
