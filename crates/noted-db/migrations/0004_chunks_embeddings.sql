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
    content_hash text PRIMARY KEY REFERENCES chunks(content_hash) ON DELETE CASCADE,
    model_id     text NOT NULL,
    -- 768 = bge-base-en-v1.5. The dimension is fixed in the column because
    -- pgvector cannot index a dimensionless vector. Changing the embedding model
    -- therefore requires a migration AND a full re-embed — deliberate, not an
    -- oversight. See spec §5.2.
    embedding    vector(768) NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- HNSW for cosine similarity — M1c's vector arm and the related-notes panel.
-- Requires pgvector >= 0.8 for iterative index scans under a WHERE filter
-- (permission-filtered retrieval); /health enforces that floor at runtime.
CREATE INDEX embeddings_hnsw_idx ON embeddings
    USING hnsw (embedding vector_cosine_ops);

-- The dirty-set query joins blocks -> chunks on content_hash.
CREATE INDEX chunks_created_at_idx ON chunks (created_at);
