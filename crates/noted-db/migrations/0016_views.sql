-- A VIEW is a saved way of looking at a collection: which kind of view, which
-- filters, which sorts, which columns.
--
-- `config` is JSON for the same reason `collection_properties.config` is: a
-- board's group-by property, a calendar's date property and a table's column
-- widths have nothing in common, and giving each its own column would widen
-- this table every time a view type is added.
--
-- Filters and sorts are stored as JSON but COMPILED TO SQL when the view runs
-- (see `noted_db::views`). Storing them as text to be interpolated would be an
-- injection hole; storing them structured means the compiler can bind
-- parameters and reject anything it does not recognise.
CREATE TABLE collection_views (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    collection_id uuid NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    name          text NOT NULL,
    kind          text NOT NULL,          -- 'table' | 'board' | 'calendar' | 'gallery'
    config        jsonb NOT NULL DEFAULT '{}'::jsonb,
    filters       jsonb NOT NULL DEFAULT '[]'::jsonb,
    sorts         jsonb NOT NULL DEFAULT '[]'::jsonb,
    position      int  NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX collection_views_collection_idx ON collection_views (collection_id, position);
