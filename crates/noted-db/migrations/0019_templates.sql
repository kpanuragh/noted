-- A template is a saved page shape: its title pattern, its blocks, and (when it
-- came from a database row) its property values.
--
-- Stored as its own rows rather than as a flag on `pages`, because a template
-- is NOT a page: it must not appear in the tree, in search, in the graph, or in
-- a view's row list. Making it a page with `is_template = true` would mean
-- every one of those queries needs a new predicate, and the one that forgets it
-- leaks a template into a user's notes — the same "one query disagrees" failure
-- this codebase has produced four times.
CREATE TABLE templates (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id  uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name          text NOT NULL,
    -- May contain {{variables}}, substituted at instantiation.
    title_pattern text NOT NULL,
    -- The blocks, in order: [{ "node_type": "paragraph", "text": "..." }, ...]
    blocks        jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- Property values to apply when instantiated into a collection.
    properties    jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX templates_workspace_idx ON templates (workspace_id);
