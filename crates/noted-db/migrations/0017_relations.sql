-- Relation links between database rows, and rollups over them.
--
-- A relation VALUE could have lived in `page_properties.value` as an array of
-- uuids, and that was the first design. It does not survive contact with the
-- requirement that relations are BIDIRECTIONAL: with the links inside a JSON
-- blob on one side, "which rows point at me" is a scan of every value in the
-- workspace, and keeping the mirror side consistent is application code that a
-- crash can leave half-done.
--
-- So links are rows. One row per (property, from, to), with indexes both ways,
-- and the mirror is a database CASCADE rather than a second write anyone could
-- forget.
CREATE TABLE page_relations (
    property_id uuid NOT NULL REFERENCES collection_properties(id) ON DELETE CASCADE,
    from_page   uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    to_page     uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (property_id, from_page, to_page)
);

-- The PK covers "what does this row point at". This covers the other direction,
-- which is what makes a relation bidirectional without a mirror table.
CREATE INDEX page_relations_to_idx ON page_relations (to_page, property_id);

-- A rollup is a property whose value is COMPUTED from a relation rather than
-- stored. `config` names the relation property, the target property, and the
-- function — all as ids, so renaming a column cannot break a rollup.
--
-- No new table: a rollup IS a `collection_properties` row with kind='rollup',
-- which means it appears in views, filters and sorts with no special-casing
-- anywhere. The value is materialised into `page_properties` when the related
-- set changes, so a table of 500 rows is one query rather than 500.
