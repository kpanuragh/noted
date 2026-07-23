import { describe, it, expect } from "vitest";
import { getSchema } from "@tiptap/core";
import { isSafeHref, CleanLink, editorExtensions, ALLOWED_PROTOCOLS } from "./editorExtensions";

describe("isSafeHref", () => {
  it("allows the schemes a note actually needs", () => {
    expect(isSafeHref("https://en.wikipedia.org/wiki/Reptile")).toBe(true);
    expect(isSafeHref("http://localhost:3000/page")).toBe(true);
    expect(isSafeHref("mailto:kp@example.com")).toBe(true);
    // Relative and in-page links carry no scheme and cannot execute.
    expect(isSafeHref("/pages/abc")).toBe(true);
    expect(isSafeHref("#section")).toBe(true);
  });

  it("rejects executable schemes", () => {
    // These are rendered as clickable anchors and the href comes from whatever
    // was pasted, so this is a real injection surface rather than a
    // theoretical one.
    expect(isSafeHref("javascript:alert(1)")).toBe(false);
    expect(isSafeHref("data:text/html,<script>alert(1)</script>")).toBe(false);
    expect(isSafeHref("vbscript:msgbox(1)")).toBe(false);
  });

  it("is not fooled by case or leading whitespace", () => {
    // Both are how this check is usually got around: the browser tolerates
    // them, a naive `startsWith("javascript:")` does not catch them.
    expect(isSafeHref("JaVaScRiPt:alert(1)")).toBe(false);
    expect(isSafeHref("  javascript:alert(1)")).toBe(false);
    expect(isSafeHref("\tjavascript:alert(1)")).toBe(false);
  });

  it("rejects nothing-shaped input rather than guessing", () => {
    expect(isSafeHref(null)).toBe(false);
    expect(isSafeHref(undefined)).toBe(false);
    expect(isSafeHref("")).toBe(false);
    expect(isSafeHref("   ")).toBe(false);
    expect(isSafeHref("not a url at all")).toBe(false);
  });

  it("keeps the allowlist to schemes that cannot execute", () => {
    // A guard against someone widening this later without thinking: the point
    // of an allowlist is that adding to it is a deliberate act.
    expect([...ALLOWED_PROTOCOLS]).toEqual(["http", "https", "mailto"]);
  });
});

describe("CleanLink schema", () => {
  it("declares href and nothing else", () => {
    // THE BUG THIS EXISTS FOR. Tiptap's Link declares href, target, rel AND
    // class, and parses all four off a pasted <a>. Pasting a Wikipedia article
    // stored marks carrying class="null", rel="mw:WikiLink" and title="..." —
    // none of which describe the user's document. They describe the page it
    // was copied from.
    const attrs = CleanLink.config.addAttributes?.call({
      ...CleanLink,
      parent: undefined,
      options: CleanLink.options,
    } as never);

    expect(Object.keys(attrs ?? {})).toEqual(["href"]);
  });

  it("drops an unsafe href at parse time instead of storing it", () => {
    const attrs = CleanLink.config.addAttributes?.call({
      ...CleanLink,
      parent: undefined,
      options: CleanLink.options,
    } as never) as { href: { parseHTML: (el: { getAttribute: (n: string) => string | null }) => unknown } };

    const el = (href: string) => ({ getAttribute: (n: string) => (n === "href" ? href : null) });

    expect(attrs.href.parseHTML(el("https://example.com"))).toBe("https://example.com");
    // Rejected at the point of PARSING, so an unsafe URL never enters the
    // document at all — rather than being stored and filtered at render time,
    // where every future renderer would have to remember to filter it.
    expect(attrs.href.parseHTML(el("javascript:alert(1)"))).toBe(null);
  });
});

describe("editorExtensions", () => {
  // The COMPOSED schema, not the extension list. Asserting on
  // `editorExtensions().map(e => e.name)` looks like it tests this and does
  // not: StarterKit is a single entry whose bundled extensions never appear as
  // top-level names, so that assertion passes whether or not the bundled Link
  // is disabled. It was written first, and mutation testing caught it —
  // removing `link: false` left it green.
  const schema = () => getSchema(editorExtensions());

  it("resolves a link mark carrying only href", () => {
    // StarterKit 3.x bundles Tiptap's Link. Leave it enabled and its mark wins,
    // reinstating the class/rel/target attributes CleanLink exists to drop —
    // a fix that appears applied and is not.
    expect(Object.keys(schema().marks.link.spec.attrs ?? {})).toEqual(["href"]);
  });

  it("still supports the formatting a note needs", () => {
    // The sanitiser must not have cost the markup itself. "No HTML" is not
    // "no formatting".
    const s = schema();
    for (const mark of ["bold", "italic", "code", "link"]) {
      expect(s.marks[mark], `missing mark: ${mark}`).toBeTruthy();
    }
    for (const node of ["heading", "bulletList", "orderedList", "blockquote", "codeBlock"]) {
      expect(s.nodes[node], `missing node: ${node}`).toBeTruthy();
    }
  });

  it("renders links with noopener, and does not take rel from the pasted source", () => {
    const link = editorExtensions().find((e) => e.name === "link");
    const rel = (link?.options as { HTMLAttributes?: Record<string, string> })
      ?.HTMLAttributes?.rel;
    // Render-time presentation, not parsed content: it cannot vary with
    // wherever a paste came from.
    expect(rel).toContain("noopener");
  });
});
