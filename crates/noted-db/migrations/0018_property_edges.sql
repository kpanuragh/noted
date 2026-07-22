-- Property-derived graph edges carry a provenance that chunk-derived ones
-- cannot: they come from a structured value a person chose, not from prose a
-- model guessed at.
--
-- `edges.source_chunk_hash` is NOT NULL and REFERENCES chunks, so a
-- property-derived edge has nowhere to live in that table without inventing a
-- fake chunk. Rather than weaken a constraint that has caught real bugs, these
-- get their own table with the same shape and their own provenance column.
--
-- Reads union the two. That keeps the chunk-provenance invariant intact — every
-- row in `edges` really is evidenced by a chunk — while letting a select value
-- or a relation contribute to the graph.
CREATE TABLE property_edges (
    workspace_id  uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    source_entity uuid NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    target_entity uuid NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    relation      text NOT NULL,
    weight        real NOT NULL DEFAULT 1.0,
    -- The property the edge was derived from. ON DELETE CASCADE means deleting
    -- a column retracts its edges, which is the property-side equivalent of
    -- what `replace_chunk_edges` does for prose.
    property_id   uuid NOT NULL REFERENCES collection_properties(id) ON DELETE CASCADE,
    -- The page whose value produced it, so archiving a page can retract it.
    page_id       uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    PRIMARY KEY (source_entity, target_entity, relation, property_id, page_id)
);

CREATE INDEX property_edges_workspace_idx ON property_edges (workspace_id);
CREATE INDEX property_edges_page_idx ON property_edges (page_id);
