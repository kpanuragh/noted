import { describe, expect, it } from "vitest";
import { isCellSelection, shouldShowTableMenu, shouldShowTextMenu } from "./bubbleMenus";

// The four selection shapes the editor can actually be in.
const caret = { empty: true };
const text = { empty: false };
const cells = { empty: false, $anchorCell: {} };
const nodeSel = { empty: false };

describe("bubble menu visibility", () => {
  it("shows text formatting for a text selection outside a table", () => {
    expect(shouldShowTextMenu(text)).toBe(true);
    expect(shouldShowTableMenu(false, text)).toBe(false);
  });

  it("shows nothing for a bare caret outside a table", () => {
    expect(shouldShowTextMenu(caret)).toBe(false);
    expect(shouldShowTableMenu(false, caret)).toBe(false);
  });

  it("shows table tools for a caret inside a table", () => {
    // The case a selection-driven menu cannot cover: you add a row without
    // selecting anything first.
    expect(shouldShowTableMenu(true, caret)).toBe(true);
    expect(shouldShowTextMenu(caret)).toBe(false);
  });

  it("shows text formatting, not table tools, when selecting text in a cell", () => {
    expect(shouldShowTextMenu(text)).toBe(true);
    expect(shouldShowTableMenu(true, text)).toBe(false);
  });

  it("shows table tools, not text formatting, when whole cells are selected", () => {
    expect(shouldShowTableMenu(true, cells)).toBe(true);
    expect(shouldShowTextMenu(cells)).toBe(false);
  });

  it("never shows both menus at once, for any selection in or out of a table", () => {
    // The invariant the split exists to hold. Two menus anchored to the same
    // rectangle overlap, and the overlap is unreadable rather than merely ugly.
    for (const inTable of [true, false]) {
      for (const selection of [caret, text, cells, nodeSel]) {
        expect(shouldShowTextMenu(selection) && shouldShowTableMenu(inTable, selection)).toBe(false);
      }
    }
  });

  it("does not mistake an ordinary selection for a cell selection", () => {
    expect(isCellSelection(text)).toBe(false);
    expect(isCellSelection(null)).toBe(false);
    expect(isCellSelection(undefined)).toBe(false);
    expect(isCellSelection(cells)).toBe(true);
  });
});
