-- Databases: a COLLECTION is a set of pages with typed properties.
--
-- Pages already exist and already carry a tree, so a collection does not
-- introduce a second kind of content — it introduces a typed LAYER over pages
-- that are already there. A row in a database IS a page, which is what makes a
-- database row openable as a document.
CREATE TABLE collections (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The page the collection is displayed on. Deleting that page deletes the
    -- collection, which is what a user means by deleting a database.
    page_id      uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    name         text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX collections_workspace_idx ON collections (workspace_id);
CREATE UNIQUE INDEX collections_page_idx ON collections (page_id);

-- A property is a typed column.
--
-- `kind` is text, not an enum: property types will grow (formula, person,
-- file), and an enum makes each addition a migration in a hot table. The set is
-- validated in the repository, which is also where the value-coercion logic
-- that depends on it lives — one place, not two.
--
-- `config` carries kind-specific settings as JSON: the options of a select, the
-- target collection of a relation. Keeping it out of columns means adding a
-- property type does not widen this table.
CREATE TABLE collection_properties (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    collection_id uuid NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    name          text NOT NULL,
    kind          text NOT NULL,
    config        jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Display order. A float would avoid renumbering on reorder, but an int is
    -- honest about what it is and reordering a handful of columns is cheap.
    position      int  NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (collection_id, name)
);
CREATE INDEX collection_properties_collection_idx
    ON collection_properties (collection_id, position);

-- One page's value for one property.
--
-- ON DELETE CASCADE on `property_id` is the whole answer to "what happens to the
-- values when a column is deleted" — the database does it, rather than a
-- repository function that someone could forget to call.
--
-- The value is JSON rather than a per-type column because a typed union across
-- eight kinds is either eight nullable columns (seven of them always NULL) or a
-- table per kind. JSON keeps one row per (page, property) and puts the type
-- discipline in the writer, where `kind` already lives.
CREATE TABLE page_properties (
    page_id     uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    property_id uuid NOT NULL REFERENCES collection_properties(id) ON DELETE CASCADE,
    value       jsonb NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (page_id, property_id)
);

-- Filtering and sorting a view means "every page in this collection, ordered by
-- this property", so the property side is the leading column.
CREATE INDEX page_properties_property_idx ON page_properties (property_id);
