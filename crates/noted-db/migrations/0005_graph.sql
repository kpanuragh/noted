-- Entities are nodes in one tenant's knowledge graph. "Postgres" in workspace
-- A and "Postgres" in workspace B are DIFFERENT graph nodes even though the
-- text matches — uniqueness is scoped to (workspace_id, name), not global.
-- This partitions the graph by tenant from day one, for the same
-- permission-aware-retrieval reason the whole project uses one database.
CREATE TABLE entities (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name         text NOT NULL,
    entity_type  text NOT NULL,
    description  text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);
CREATE INDEX entities_workspace_idx ON entities (workspace_id);

-- Every edge carries source_chunk_hash: provenance. An edge is owned by the
-- chunk it was extracted from, which makes incremental re-extraction a scoped
-- DELETE WHERE source_chunk_hash=$1 AND model_id=$2 instead of a global
-- recompute. ON DELETE CASCADE on source_chunk_hash means deleting a chunk
-- (e.g. because the page that referenced it was rewritten) removes the edges
-- it sourced too.
CREATE TABLE edges (
    source_entity     uuid NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    target_entity     uuid NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    relation          text NOT NULL,
    weight            real NOT NULL DEFAULT 1.0,
    source_chunk_hash text NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    model_id          text NOT NULL,
    PRIMARY KEY (source_entity, target_entity, relation, source_chunk_hash, model_id)
);
CREATE INDEX edges_source_chunk_idx ON edges (source_chunk_hash, model_id);
CREATE INDEX edges_source_entity_idx ON edges (source_entity);
CREATE INDEX edges_target_entity_idx ON edges (target_entity);

-- The "this chunk is extracted under this model" marker. Two models coexist,
-- exactly like embeddings (M1b): keyed (content_hash, model_id).
CREATE TABLE chunk_extractions (
    content_hash text NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    model_id     text NOT NULL,
    extracted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (content_hash, model_id)
);
