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
