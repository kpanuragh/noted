CREATE TABLE doc_updates (
    page_id uuid   NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    seq     bigint NOT NULL,
    update  bytea  NOT NULL,
    PRIMARY KEY (page_id, seq)
);

-- Per-page monotonic sequence. A single sequence table keeps `seq` assignment
-- inside the same transaction as the insert, so compaction can renumber safely.
CREATE TABLE doc_seq (
    page_id uuid PRIMARY KEY REFERENCES pages(id) ON DELETE CASCADE,
    next    bigint NOT NULL DEFAULT 0
);
