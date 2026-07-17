-- Chunks are CONTENT-ADDRESSED: the key is the hash of the text, not a block
-- reference. A block that moves keeps its hash and re-embeds nothing; identical
-- text on ten pages is one row. `blocks` rows have no stable identity (their PK
-- is (page_id, block_index) and the projection rewrites them wholesale), so
-- content addressing is the only durable handle we have.
CREATE TABLE chunks (
    content_hash   text PRIMARY KEY,
    text           text NOT NULL,
    token_estimate int  NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now()
);

-- Which chunks a page CURRENTLY has. Mirrors `blocks`: rewritten wholesale per
-- page on each rechunk.
--
-- This table is load-bearing and non-obvious. A chunk's hash is the hash of the
-- CHUNK's text — and chunking merges short blocks and splits long ones, so a
-- chunk's text is generally NOT any single block's text. `chunks.content_hash`
-- and `blocks.content_hash` are therefore DIFFERENT HASH SPACES that never join.
-- Liveness ("is this chunk still referenced by a real page?") can only come from
-- an explicit link, which is this table.
CREATE TABLE page_chunks (
    page_id      uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    chunk_index  int  NOT NULL,
    content_hash text NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    PRIMARY KEY (page_id, chunk_index)
);

CREATE INDEX page_chunks_hash_idx ON page_chunks (content_hash);

CREATE TABLE embeddings (
    content_hash text NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    model_id     text NOT NULL,
    -- 768 = bge-base-en-v1.5. The dimension is fixed in the column because
    -- pgvector cannot index a dimensionless vector. A model with different
    -- dimensions therefore needs a migration; a model with the SAME dimensions
    -- can be rolled out live — see the PK below.
    embedding    vector(768) NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    -- model_id is IN the key so two models' vectors coexist. Without it, a
    -- re-embed overwrites row by row, leaving the table half old-model and half
    -- new-model — and vectors from different models are not comparable, so
    -- search silently returns garbage for the whole migration. With it, the old
    -- model keeps serving while the new one backfills, and you cut over atomically.
    PRIMARY KEY (content_hash, model_id)
);

-- Spans all models' vectors, so every query MUST filter on model_id. Safe:
-- pgvector >= 0.8 iterative index scans handle a filtered HNSW scan without
-- overfiltering. /health enforces that floor at runtime.
CREATE INDEX embeddings_hnsw_idx ON embeddings
    USING hnsw (embedding vector_cosine_ops);

CREATE INDEX chunks_created_at_idx ON chunks (created_at);
