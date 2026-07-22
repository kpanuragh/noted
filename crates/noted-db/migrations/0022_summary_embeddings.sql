-- Embeddings of community summaries — the THIRD embedding space in the product.
--
-- Chunks have one (M1b). Entities deliberately do not (M2c design 2.1: they
-- have no text worth embedding and `description` is almost always NULL). This
-- one exists because global search was selecting themes by MEMBER COUNT, which
-- is a proxy for importance and not for relevance: a question about a niche
-- topic mapped over the workspace's largest themes, which may not include it.
--
-- Keyed `(community_id, model_id)` for exactly the reason `embeddings` is:
-- two models coexist during a migration, and a summary embedded by one must
-- never be compared against a question embedded by the other.
CREATE TABLE community_summary_embeddings (
    community_id uuid NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
    model_id     text NOT NULL,
    -- The hash of the summary text this vector was made from. A summary that
    -- has been regenerated leaves a stale vector behind, and comparing the
    -- hashes is how the worker knows to redo it — the same content-addressed
    -- trick chunk embeddings use.
    summary_hash text NOT NULL,
    embedding    vector(768) NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (community_id, model_id)
);

-- Communities per workspace are few (tens, not millions), so an HNSW index
-- would cost more to maintain than the sequential scan it replaces. Deliberate:
-- pgvector's exact search over a hundred rows is faster than an approximate one,
-- and it cannot silently under-return the way HNSW can.
CREATE INDEX community_summary_embeddings_model_idx
    ON community_summary_embeddings (model_id);
