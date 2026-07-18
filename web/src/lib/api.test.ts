import { describe, expect, it, vi, beforeEach } from "vitest";
import { api } from "./api";

beforeEach(() => vi.restoreAllMocks());

describe("api.listPages", () => {
  it("omits parent_id from the query when listing root pages", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => [],
    });
    vi.stubGlobal("fetch", fetchMock);

    await api.listPages("ws-1");

    const url = new URL(fetchMock.mock.calls[0][0]);
    expect(url.searchParams.get("workspace_id")).toBe("ws-1");
    expect(url.searchParams.has("parent_id")).toBe(false);
  });

  it("throws on a non-ok response rather than returning undefined", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 500 }));
    await expect(api.listPages("ws-1")).rejects.toThrow(/500/);
  });
});

describe("api.recentPages", () => {
  it("requests the recent endpoint with the workspace and limit", async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, json: async () => [] });
    vi.stubGlobal("fetch", fetchMock);

    await api.recentPages("ws-1", 5);

    const url = new URL(fetchMock.mock.calls[0][0]);
    expect(url.pathname).toBe("/api/pages/recent");
    expect(url.searchParams.get("workspace_id")).toBe("ws-1");
    expect(url.searchParams.get("limit")).toBe("5");
  });

  it("omits limit when not given, letting the server pick the default", async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, json: async () => [] });
    vi.stubGlobal("fetch", fetchMock);

    await api.recentPages("ws-1");

    const url = new URL(fetchMock.mock.calls[0][0]);
    expect(url.searchParams.has("limit")).toBe(false);
  });

  it("returns the pages, preserving the server's newest-first order", async () => {
    // The fixture is built fresh inside the mock and the expectation is a
    // literal: an earlier version of this test compared the result against the
    // same array object the mock returned, so a client that called
    // `.reverse()` (which mutates in place) reversed the expectation too and
    // the test passed vacuously.
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => [
          { id: "b", workspace_id: "ws-1", parent_id: null, title: "Newer",
            created_at: "2026-07-18T10:00:00Z", updated_at: "2026-07-18T11:00:00Z" },
          { id: "a", workspace_id: "ws-1", parent_id: null, title: "Older",
            created_at: "2026-07-01T10:00:00Z", updated_at: "2026-07-02T11:00:00Z" },
        ],
      }),
    );

    const result = await api.recentPages("ws-1");

    expect(result.map((p) => p.id)).toEqual(["b", "a"]);
    expect(result.map((p) => p.updated_at)).toEqual([
      "2026-07-18T11:00:00Z",
      "2026-07-02T11:00:00Z",
    ]);
  });

  it("throws when the endpoint is not deployed yet (404)", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 404 }));
    await expect(api.recentPages("ws-1")).rejects.toThrow(/404/);
  });
});

describe("api.workspaceStats", () => {
  it("puts the workspace id in the path, not the query string", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ pages: 0, chunks_indexed: 0, entities: 0, edges: 0 }),
    });
    vi.stubGlobal("fetch", fetchMock);

    // The id is generated per-run rather than written as a literal. An earlier
    // version passed "ws-42" and asserted "/api/workspaces/ws-42/stats", so an
    // implementation that ignored its argument and hardcoded that exact path
    // passed the test. The expectation is now derived from the argument, and
    // the argument is unguessable, so only a client that actually interpolates
    // what it was given can pass.
    const workspaceId = `ws-${Math.random().toString(36).slice(2, 10)}`;
    await api.workspaceStats(workspaceId);

    const url = new URL(fetchMock.mock.calls[0][0]);
    expect(url.pathname).toBe(`/api/workspaces/${workspaceId}/stats`);
    expect(url.searchParams.has("workspace_id")).toBe(false);
  });

  it("percent-encodes a workspace id that would otherwise alter the path", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ pages: 0, chunks_indexed: 0, entities: 0, edges: 0 }),
    });
    vi.stubGlobal("fetch", fetchMock);

    await api.workspaceStats("a/../../evil");

    const url = new URL(fetchMock.mock.calls[0][0]);
    // Unencoded, "a/../../evil" collapses via URL normalisation and the request
    // escapes /api/workspaces entirely.
    expect(url.pathname).toBe("/api/workspaces/a%2F..%2F..%2Fevil/stats");
  });

  it("returns the four counters", async () => {
    const stats = { pages: 12, chunks_indexed: 340, entities: 412, edges: 1204 };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, json: async () => stats }));

    await expect(api.workspaceStats("ws-1")).resolves.toEqual(stats);
  });

  it("throws when the endpoint is not deployed yet (404)", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 404 }));
    await expect(api.workspaceStats("ws-1")).rejects.toThrow(/404/);
  });
});

/**
 * A 200 whose body is not the documented shape — the realistic form of API
 * version skew. Without a runtime check these bodies flow through the `as T`
 * cast into React and throw during render, which no `.catch()` in a panel can
 * intercept: the panel's promise already resolved. Rejecting at the boundary
 * turns every one of these into an ordinary error the panels already handle.
 */
describe("shape validation of 200 responses", () => {
  const ok = (body: unknown) =>
    vi.fn().mockResolvedValue({ ok: true, json: async () => body });

  it("rejects a 200 whose body is an object where a list belongs", async () => {
    // The exact payload that used to reach RecentPages and throw on .map().
    vi.stubGlobal("fetch", ok({ pages: [] }));
    await expect(api.recentPages("ws-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("rejects a 200 whose body is null where a list belongs", async () => {
    vi.stubGlobal("fetch", ok(null));
    await expect(api.recentPages("ws-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("rejects a list whose elements are not pages", async () => {
    vi.stubGlobal("fetch", ok([{ id: "p-1" }]));
    await expect(api.recentPages("ws-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("rejects a page whose timestamps are numbers instead of strings", async () => {
    // A backend that switched to epoch millis: formatRelativeTime would render
    // "unknown" forever rather than anyone noticing the skew.
    vi.stubGlobal(
      "fetch",
      ok([
        { id: "p-1", workspace_id: "ws-1", parent_id: null, title: "T",
          created_at: 1784388717223, updated_at: 1784388717223 },
      ]),
    );
    await expect(api.recentPages("ws-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("accepts a well-formed page list unchanged", async () => {
    const body = [
      { id: "p-1", workspace_id: "ws-1", parent_id: null, title: "T",
        created_at: "2026-07-18T10:00:00Z", updated_at: "2026-07-18T11:00:00Z" },
    ];
    vi.stubGlobal("fetch", ok(body));
    await expect(api.recentPages("ws-1")).resolves.toEqual(body);
  });

  it("rejects a 200 with body null where stats belong, instead of resolving null", async () => {
    // This is the "Loading… forever" bug: null resolved successfully and was
    // indistinguishable from the not-yet-loaded sentinel.
    vi.stubGlobal("fetch", ok(null));
    await expect(api.workspaceStats("ws-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("rejects stats that are missing a counter", async () => {
    vi.stubGlobal("fetch", ok({ pages: 1, chunks_indexed: 2, entities: 3 }));
    await expect(api.workspaceStats("ws-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("rejects stats whose counters are strings", async () => {
    vi.stubGlobal("fetch", ok({ pages: "1", chunks_indexed: 2, entities: 3, edges: 4 }));
    await expect(api.workspaceStats("ws-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("rejects stats whose counters are NaN", async () => {
    vi.stubGlobal("fetch", ok({ pages: NaN, chunks_indexed: 2, entities: 3, edges: 4 }));
    await expect(api.workspaceStats("ws-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("accepts well-formed stats unchanged", async () => {
    const body = { pages: 12, chunks_indexed: 340, entities: 412, edges: 1204 };
    vi.stubGlobal("fetch", ok(body));
    await expect(api.workspaceStats("ws-1")).resolves.toEqual(body);
  });

  it("rejects a malformed single page from getPage", async () => {
    vi.stubGlobal("fetch", ok({ id: "p-1", title: "T" }));
    await expect(api.getPage("p-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("rejects a malformed page from createPage", async () => {
    vi.stubGlobal("fetch", ok("not a page"));
    await expect(api.createPage("ws-1", null, "T")).rejects.toThrow(/unexpected shape/i);
  });

  it("rejects malformed search, quickfind and related payloads", async () => {
    vi.stubGlobal("fetch", ok([{ page_id: "p-1" }]));
    await expect(api.search("ws-1", "q")).rejects.toThrow(/unexpected shape/i);
    await expect(api.quickFind("ws-1", "q")).rejects.toThrow(/unexpected shape/i);
    await expect(api.related("p-1")).rejects.toThrow(/unexpected shape/i);
  });

  it("names the endpoint in the error so skew is diagnosable from a log", async () => {
    vi.stubGlobal("fetch", ok({}));
    await expect(api.recentPages("ws-1")).rejects.toThrow(/\/api\/pages\/recent/);
  });
});
