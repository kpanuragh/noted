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

/**
 * How far behind the background indexer is for a workspace.
 *
 * A surface that cannot do its job yet — global search with no summarised
 * themes, search missing a note written seconds ago — should say how far along
 * indexing is, rather than looking broken or empty.
 */
export type IndexingStatus = {
  embedded: number;
  embed_total: number;
  extracted: number;
  extract_total: number;
  summarised: number;
  summary_total: number;
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


/** Why a chunk is in a local answer's evidence — the "show your work" surface. */
export type Inclusion =
  | { kind: "seed" }
  | { kind: "graph"; hops: number };

export type Citation = {
  page_id: string;
  title: string;
  content_hash: string;
  snippet: string;
  why: Inclusion;
};

export type SeedEntity = { id: string; name: string };

export type LocalAnswer = {
  answer: string;
  citations: Citation[];
  seed_entities: SeedEntity[];
};

export type PartialAnswer = {
  community_id: string;
  member_count: number;
  was_stale: boolean;
  text: string;
  relevance: number;
};

export type GlobalAnswer = {
  answer: string;
  partials: PartialAnswer[];
  /** Themes with no usable summary, so NOT consulted. Shown, never hidden. */
  skipped_unsummarised: number;
};


/**
 * Raised when the API says we have no live session.
 *
 * Separate from `ApiShapeError` and from a generic failure because the UI's
 * response is different in kind: not "try again", but "sign in". Panels catch
 * it and the app redirects rather than rendering a broken page.
 */
export class UnauthorizedError extends Error {
  constructor() {
    super("not signed in");
    this.name = "UnauthorizedError";
  }
}

/**
 * Raised when the thing asked for is not there.
 *
 * Separate for the same reason `UnauthorizedError` is: the caller's response
 * differs in KIND. A generic failure means "try again"; this means "it is
 * gone", and a surface that cannot tell them apart shows a note that no longer
 * exists as one that is still loading — forever.
 */
export class NotFoundError extends Error {
  constructor() {
    super("not found");
    this.name = "NotFoundError";
  }
}

export type Workspace = {
  id: string;
  name: string;
  role: string;
};

function isWorkspace(v: unknown): v is Workspace {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  return typeof o.id === "string" && typeof o.name === "string";
}

export type Me = {
  id: string;
  email: string;
  display_name: string;
  created_at: string;
};

function isMe(v: unknown): v is Me {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.id === "string" &&
    typeof o.email === "string" &&
    typeof o.display_name === "string"
  );
}

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

function isIndexingStatus(v: unknown): v is IndexingStatus {
  return (
    isRecord(v) &&
    isFiniteNumber(v.embedded) &&
    isFiniteNumber(v.embed_total) &&
    isFiniteNumber(v.extracted) &&
    isFiniteNumber(v.extract_total) &&
    isFiniteNumber(v.summarised) &&
    isFiniteNumber(v.summary_total)
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
  // 401 is not "an error occurred", it is "you are signed out" — the caller's
  // response is a redirect, not a retry. Distinguished here so no panel has to
  // sniff a status code out of a message string.
  if (res.status === 401) throw new UnauthorizedError();
  // Likewise 404: "this is gone" is not "this failed, retry".
  if (res.status === 404) throw new NotFoundError();
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

/**
 * Validate serde's INTERNALLY-tagged enum: `#[serde(tag = "kind", rename_all =
 * "snake_case")]` puts the variant in a `kind` field, so the wire shape is
 * `{kind:"seed"}` / `{kind:"graph",hops:n}` — identical to the `Inclusion` type
 * above, which is why nothing needs remapping.
 *
 * Worth stating because the first version of this guard was written for serde's
 * EXTERNALLY-tagged default (`"Seed"` / `{Graph:{hops}}`) and would therefore
 * have rejected every genuine response as a shape error — the Ask page would
 * have failed 100% of the time, and `tsc` cannot see the difference because
 * both are `unknown` at the boundary. Verified against the running server, not
 * against the type.
 */
function parseInclusion(v: unknown): Inclusion | null {
  if (typeof v !== "object" || v === null) return null;
  const o = v as Record<string, unknown>;
  if (o.kind === "seed") return { kind: "seed" };
  if (o.kind === "graph" && typeof o.hops === "number") {
    return { kind: "graph", hops: o.hops };
  }
  return null;
}

function isLocalAnswer(v: unknown): v is LocalAnswer {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  if (typeof o.answer !== "string") return false;
  if (!Array.isArray(o.citations) || !Array.isArray(o.seed_entities)) return false;
  return o.citations.every((c) => {
    if (typeof c !== "object" || c === null) return false;
    const r = c as Record<string, unknown>;
    return (
      typeof r.page_id === "string" &&
      typeof r.title === "string" &&
      typeof r.snippet === "string" &&
      parseInclusion(r.why) !== null
    );
  });
}

function isGlobalAnswer(v: unknown): v is GlobalAnswer {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.answer === "string" &&
    Array.isArray(o.partials) &&
    typeof o.skipped_unsummarised === "number" &&
    o.partials.every((p) => {
      if (typeof p !== "object" || p === null) return false;
      const r = p as Record<string, unknown>;
      return (
        typeof r.community_id === "string" &&
        typeof r.text === "string" &&
        typeof r.relevance === "number" &&
        typeof r.member_count === "number"
      );
    })
  );
}

function arrayOf<T>(guard: (v: unknown) => v is T) {
  return (v: unknown): v is T[] => Array.isArray(v) && v.every(guard);
}

export const api = {
  async listPages(workspaceId: string, parentId?: string): Promise<Page[]> {
    const url = new URL("/api/pages", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    if (parentId) url.searchParams.set("parent_id", parentId);
    return json(await fetch(url.toString(), { credentials: "include" }), "/api/pages", arrayOf(isPage));
  },

  /**
   * Pages ordered by true last-edit time, newest first. Distinct from
   * `listPages`, which is the hierarchical tree ordered by `created_at`.
   */
  async recentPages(workspaceId: string, limit?: number): Promise<Page[]> {
    const url = new URL("/api/pages/recent", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    if (limit !== undefined) url.searchParams.set("limit", String(limit));
    return json(await fetch(url.toString(), { credentials: "include" }), "/api/pages/recent", arrayOf(isPage));
  },

  async indexing(workspaceId: string): Promise<IndexingStatus> {
    const url = new URL(
      `/api/workspaces/${encodeURIComponent(workspaceId)}/indexing`,
      API_BASE,
    );
    return json(
      await fetch(url.toString(), { credentials: "include" }),
      "/api/workspaces/:id/indexing",
      isIndexingStatus,
    );
  },

  async workspaceStats(workspaceId: string): Promise<WorkspaceStats> {
    // Encoded because the id is caller-supplied and lands in a path segment.
    // UUIDs are harmless, but an id containing "/" or ".." would silently
    // retarget the request to a different endpoint.
    const url = new URL(
      `/api/workspaces/${encodeURIComponent(workspaceId)}/stats`,
      API_BASE,
    );
    return json(await fetch(url.toString(), { credentials: "include" }), "/api/workspaces/:id/stats", isWorkspaceStats);
  },

  async getPage(id: string): Promise<Page> {
    const url = new URL(`/api/pages/${encodeURIComponent(id)}`, API_BASE);
    return json(await fetch(url.toString(), { credentials: "include" }), "/api/pages/:id", isPage);
  },

  async createPage(
    workspaceId: string,
    parentId: string | null,
    title: string,
  ): Promise<Page> {
    const res = await fetch(new URL("/api/pages", API_BASE).toString(), {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ workspace_id: workspaceId, parent_id: parentId, title }),
    });
    return json(res, "POST /api/pages", isPage);
  },

  async quickFind(workspaceId: string, q: string): Promise<QuickHit[]> {
    const url = new URL("/api/quickfind", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    url.searchParams.set("q", q);
    return json(await fetch(url.toString(), { credentials: "include" }), "/api/quickfind", arrayOf(isQuickHit));
  },

  async search(workspaceId: string, q: string): Promise<SearchHit[]> {
    const url = new URL("/api/search", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    url.searchParams.set("q", q);
    return json(await fetch(url.toString(), { credentials: "include" }), "/api/search", arrayOf(isSearchHit));
  },

  async related(pageId: string): Promise<RelatedHit[]> {
    const url = new URL(
      `/api/pages/${encodeURIComponent(pageId)}/related`,
      API_BASE,
    );
    return json(await fetch(url.toString(), { credentials: "include" }), "/api/pages/:id/related", arrayOf(isRelatedHit));
  },

  async askLocal(workspaceId: string, q: string): Promise<LocalAnswer> {
    const url = new URL("/api/ask/local", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    url.searchParams.set("q", q);
    return json<LocalAnswer>(await fetch(url.toString(), { credentials: "include" }), "ask/local", isLocalAnswer);
  },

  async askGlobal(workspaceId: string, q: string): Promise<GlobalAnswer> {
    const url = new URL("/api/ask/global", API_BASE);
    url.searchParams.set("workspace_id", workspaceId);
    url.searchParams.set("q", q);
    return json<GlobalAnswer>(await fetch(url.toString(), { credentials: "include" }), "ask/global", isGlobalAnswer);
  },

  async me(): Promise<Me> {
    return json<Me>(
      await fetch(new URL("/api/me", API_BASE).toString(), { credentials: "include" }),
      "/api/me",
      isMe,
    );
  },

  async signIn(email: string, password: string): Promise<Me> {
    const res = await fetch(new URL("/api/auth/signin", API_BASE).toString(), {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ email, password }),
    });
    return json<Me>(res, "/api/auth/signin", isMe);
  },

  async signUp(email: string, password: string, displayName?: string): Promise<Me> {
    const res = await fetch(new URL("/api/auth/signup", API_BASE).toString(), {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ email, password, display_name: displayName }),
    });
    return json<Me>(res, "/api/auth/signup", isMe);
  },

  async signOut(): Promise<void> {
    await fetch(new URL("/api/auth/signout", API_BASE).toString(), {
      method: "POST",
      credentials: "include",
    });
  },

  async workspaces(): Promise<Workspace[]> {
    return json<Workspace[]>(
      await fetch(new URL("/api/workspaces", API_BASE).toString(), { credentials: "include" }),
      "/api/workspaces",
      arrayOf(isWorkspace),
    );
  },

  async renamePage(id: string, title: string): Promise<void> {
    const res = await fetch(
      new URL(`/api/pages/${encodeURIComponent(id)}`, API_BASE).toString(),
      {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ title }),
      },
    );
    if (res.status === 401) throw new UnauthorizedError();
    if (!res.ok) throw new Error(`rename failed: ${res.status}`);
  },
};
