export const API_BASE = process.env.NEXT_PUBLIC_API_BASE ?? "http://localhost:8787";

export type Page = {
  id: string;
  workspace_id: string;
  parent_id: string | null;
  title: string;
};

export type QuickHit = {
  page_id: string;
  title: string;
  rank: number;
};

export type SearchHit = {
  page_id: string;
  title: string;
  snippet: string;
  score: number;
};

export type RelatedHit = {
  page_id: string;
  title: string;
  snippet: string;
  distance: number;
};

async function json<T>(res: Response): Promise<T> {
  if (!res.ok) throw new Error(`noted API error: ${res.status}`);
  return res.json() as Promise<T>;
}

export const api = {
  async listPages(workspaceId: string, parentId?: string): Promise<Page[]> {
    const url = new URL("/api/pages", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    if (parentId) url.searchParams.set("parent_id", parentId);
    return json<Page[]>(await fetch(url.toString()));
  },

  async getPage(id: string): Promise<Page> {
    const url = new URL(`/api/pages/${id}`, API_BASE);
    return json<Page>(await fetch(url.toString()));
  },

  async createPage(
    workspaceId: string,
    parentId: string | null,
    title: string,
  ): Promise<Page> {
    const res = await fetch(new URL("/api/pages", API_BASE).toString(), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ workspace_id: workspaceId, parent_id: parentId, title }),
    });
    return json<Page>(res);
  },

  async quickFind(workspaceId: string, q: string): Promise<QuickHit[]> {
    const url = new URL("/api/quickfind", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    url.searchParams.set("q", q);
    return json<QuickHit[]>(await fetch(url.toString()));
  },

  async search(workspaceId: string, q: string): Promise<SearchHit[]> {
    const url = new URL("/api/search", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    url.searchParams.set("q", q);
    return json<SearchHit[]>(await fetch(url.toString()));
  },

  async related(pageId: string): Promise<RelatedHit[]> {
    const url = new URL(`/api/pages/${pageId}/related`, API_BASE);
    return json<RelatedHit[]>(await fetch(url.toString()));
  },
};
