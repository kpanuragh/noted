/**
 * Which bubble menu, if either, claims the current selection.
 *
 * The editor registers two menus — text formatting and table tools — and both
 * anchor to the same selection rectangle. If both ever qualify they render on
 * top of each other, so the deciding rule lives here as plain functions rather
 * than inline in two `shouldShow` props where the pair could drift apart. The
 * invariant they exist to hold is testable: never both.
 */

/**
 * Is this a selection of whole table cells, rather than of text?
 *
 * Identified by the marker property prosemirror-tables sets on CellSelection
 * rather than by `instanceof`: the class would have to be imported from the
 * table package, and these are otherwise pure predicates over a shape.
 */
export function isCellSelection(selection: unknown): boolean {
  return Boolean((selection as { $anchorCell?: unknown } | null)?.$anchorCell);
}

/**
 * Text formatting: shown for a real text selection. It yields inside a table
 * whenever the selection covers cells rather than characters — bolding "three
 * whole cells" is not what that gesture means.
 */
export function shouldShowTextMenu(selection: { empty: boolean }): boolean {
  return !selection.empty && !isCellSelection(selection);
}

/**
 * Table tools: shown when the caret merely sits in a table — you add a row
 * without selecting anything — and when whole cells are selected, which is the
 * only time merging means anything.
 */
export function shouldShowTableMenu(
  inTable: boolean,
  selection: { empty: boolean },
): boolean {
  return inTable && (selection.empty || isCellSelection(selection));
}
