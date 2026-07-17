export const API_BASE = process.env.NEXT_PUBLIC_API_BASE ?? "http://127.0.0.1:8080";

export type Page = {
  id: string;
  workspace_id: string;
  parent_id: string | null;
  title: string;
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
};
