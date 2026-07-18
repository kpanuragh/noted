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

    await api.workspaceStats("ws-42");

    const url = new URL(fetchMock.mock.calls[0][0]);
    expect(url.pathname).toBe("/api/workspaces/ws-42/stats");
    expect(url.searchParams.has("workspace_id")).toBe(false);
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
