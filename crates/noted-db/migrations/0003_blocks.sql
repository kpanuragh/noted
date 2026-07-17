-- Derived projection of the CRDT document. Never a source of truth: it is
-- always reconstructible from doc_updates. M1b indexes from this table.
CREATE TABLE blocks (
    page_id      uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    block_index  int  NOT NULL,
    node_type    text NOT NULL,
    text         text NOT NULL,
    content_hash text NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (page_id, block_index)
);

-- Content addressing: M1b skips re-embedding when the hash is unchanged.
CREATE INDEX blocks_content_hash_idx ON blocks (content_hash);

-- Lexical retrieval arm for M1c hybrid search (spec §6.7).
CREATE INDEX blocks_fts_idx ON blocks
    USING gin (to_tsvector('english', text));
