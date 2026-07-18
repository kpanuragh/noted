-- Edges genuinely belong to a workspace: they connect two entities that are
-- themselves per-workspace (entities.workspace_id, UNIQUE (workspace_id,
-- name)). But `source_chunk_hash` is a GLOBAL, content-addressed key (no
-- workspace column on `chunks` — chunks are shared across workspaces when
-- their text is byte-identical, M1b). `replace_chunk_edges` deleted edges by
-- `(source_chunk_hash, model_id)` alone, so two workspaces extracting the
-- SAME chunk text would silently delete each other's edges for that chunk —
-- the incremental-vs-full-rebuild crown-jewel test caught this directly.
--
-- Denormalising workspace_id onto `edges` lets the DELETE (and every future
-- traversal / permission-filtered query, M2c/M4) scope by workspace directly,
-- without a join through `entities`.
ALTER TABLE edges ADD COLUMN workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE;

-- Backfill from the source entity's workspace. Every existing edge's
-- source_entity has a workspace_id (entities.workspace_id is NOT NULL), so
-- this covers every pre-existing row, including ones left behind by tests.
UPDATE edges e SET workspace_id = en.workspace_id
FROM entities en
WHERE en.id = e.source_entity;

ALTER TABLE edges ALTER COLUMN workspace_id SET NOT NULL;

-- The index the workspace-scoped DELETE in replace_chunk_edges relies on.
CREATE INDEX edges_workspace_chunk_idx ON edges (workspace_id, source_chunk_hash, model_id);
