export const API_BASE = process.env.NEXT_PUBLIC_API_BASE ?? "http://localhost:8787";

export type Page = {
  id: string;
  workspace_id: string;
  parent_id: string | null;
  title: string;
  /** RFC 3339 timestamp, as returned by the `pages` table's COLS. */
  created_at: string;
  updated_at: string;
};

/**
 * The four numbers behind the dashboard's headline: how much of the workspace
 * has actually made it into the retrieval index and the knowledge graph.
 */
export type WorkspaceStats = {
  pages: number;
  chunks_indexed: number;
  entities: number;
  edges: number;
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

/**
 * Raised when a 200 carries a body that is not the shape this client documents.
 *
 * This exists so that API version skew fails like any other network problem.
 * Casting `res.json()` to `T` made a wrong-shaped 200 a *render-time* throw
 * (iterating a non-array, reading a field off null), which the panels' own
 * `.catch()` cannot intercept — by then their promise has already resolved
 * successfully. A panel can only degrade gracefully if the failure arrives as a
 * rejected promise, so the check belongs here at the boundary.
 */
export class ApiShapeError extends Error {
  constructor(endpoint: string, detail: string) {
    super(`noted API returned an unexpected shape for ${endpoint}: ${detail}`);
    this.name = "ApiShapeError";
  }
}

/**
 * Deliberately hand-written rather than a schema library: the surface is six
 * small record types, and a dependency would cost more than it explains. These
 * are intentionally shallow — they check the fields this client actually reads,
 * and let unknown extra fields through so the backend can add fields without
 * breaking older clients.
 */
function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function isString(v: unknown): v is string {
  return typeof v === "string";
}

/** Rejects NaN and ±Infinity, which format as "NaN" in the UI. */
function isFiniteNumber(v: unknown): v is number {
  return typeof v === "number" && Number.isFinite(v);
}

function isPage(v: unknown): v is Page {
  return (
    isRecord(v) &&
    isString(v.id) &&
    isString(v.workspace_id) &&
    (v.parent_id === null || isString(v.parent_id)) &&
    isString(v.title) &&
    isString(v.created_at) &&
    isString(v.updated_at)
  );
}

function isWorkspaceStats(v: unknown): v is WorkspaceStats {
  return (
    isRecord(v) &&
    isFiniteNumber(v.pages) &&
    isFiniteNumber(v.chunks_indexed) &&
    isFiniteNumber(v.entities) &&
    isFiniteNumber(v.edges)
  );
}

function isQuickHit(v: unknown): v is QuickHit {
  return isRecord(v) && isString(v.page_id) && isString(v.title) && isFiniteNumber(v.rank);
}

function isSearchHit(v: unknown): v is SearchHit {
  return (
    isRecord(v) &&
    isString(v.page_id) &&
    isString(v.title) &&
    isString(v.snippet) &&
    isFiniteNumber(v.score)
  );
}

function isRelatedHit(v: unknown): v is RelatedHit {
  return (
    isRecord(v) &&
    isString(v.page_id) &&
    isString(v.title) &&
    isString(v.snippet) &&
    isFiniteNumber(v.distance)
  );
}

/** Describes what actually arrived, so a skew shows up usefully in a log. */
function describe(v: unknown): string {
  if (v === null) return "null";
  if (Array.isArray(v)) return `array of ${v.length}`;
  return typeof v;
}

async function json<T>(
  res: Response,
  endpoint: string,
  guard: (v: unknown) => v is T,
): Promise<T> {
  if (!res.ok) throw new Error(`noted API error: ${res.status}`);

  let body: unknown;
  try {
    body = await res.json();
  } catch {
    // A 200 with a non-JSON body (an HTML error page from a proxy, say).
    throw new ApiShapeError(endpoint, "body was not valid JSON");
  }

  if (!guard(body)) throw new ApiShapeError(endpoint, `got ${describe(body)}`);
  return body;
}

/** Lifts an element guard to a list guard, so a bad element fails the request. */
function arrayOf<T>(guard: (v: unknown) => v is T) {
  return (v: unknown): v is T[] => Array.isArray(v) && v.every(guard);
}

export const api = {
  async listPages(workspaceId: string, parentId?: string): Promise<Page[]> {
    const url = new URL("/api/pages", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    if (parentId) url.searchParams.set("parent_id", parentId);
    return json(await fetch(url.toString()), "/api/pages", arrayOf(isPage));
  },

  /**
   * Pages ordered by true last-edit time, newest first. Distinct from
   * `listPages`, which is the hierarchical tree ordered by `created_at`.
   */
  async recentPages(workspaceId: string, limit?: number): Promise<Page[]> {
    const url = new URL("/api/pages/recent", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    if (limit !== undefined) url.searchParams.set("limit", String(limit));
    return json(await fetch(url.toString()), "/api/pages/recent", arrayOf(isPage));
  },

  async workspaceStats(workspaceId: string): Promise<WorkspaceStats> {
    // Encoded because the id is caller-supplied and lands in a path segment.
    // UUIDs are harmless, but an id containing "/" or ".." would silently
    // retarget the request to a different endpoint.
    const url = new URL(
      `/api/workspaces/${encodeURIComponent(workspaceId)}/stats`,
      API_BASE,
    );
    return json(await fetch(url.toString()), "/api/workspaces/:id/stats", isWorkspaceStats);
  },

  async getPage(id: string): Promise<Page> {
    const url = new URL(`/api/pages/${encodeURIComponent(id)}`, API_BASE);
    return json(await fetch(url.toString()), "/api/pages/:id", isPage);
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
    return json(res, "POST /api/pages", isPage);
  },

  async quickFind(workspaceId: string, q: string): Promise<QuickHit[]> {
    const url = new URL("/api/quickfind", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    url.searchParams.set("q", q);
    return json(await fetch(url.toString()), "/api/quickfind", arrayOf(isQuickHit));
  },

  async search(workspaceId: string, q: string): Promise<SearchHit[]> {
    const url = new URL("/api/search", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    url.searchParams.set("q", q);
    return json(await fetch(url.toString()), "/api/search", arrayOf(isSearchHit));
  },

  async related(pageId: string): Promise<RelatedHit[]> {
    const url = new URL(
      `/api/pages/${encodeURIComponent(pageId)}/related`,
      API_BASE,
    );
    return json(await fetch(url.toString()), "/api/pages/:id/related", arrayOf(isRelatedHit));
  },
};
