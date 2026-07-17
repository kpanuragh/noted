CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE workspaces (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE pages (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    parent_id    uuid REFERENCES pages(id) ON DELETE CASCADE,
    title        text NOT NULL DEFAULT 'Untitled',
    archived_at  timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX pages_workspace_parent_idx ON pages (workspace_id, parent_id);
CREATE INDEX pages_title_trgm_idx ON pages USING gin (title gin_trgm_ops);

-- A deterministic default workspace so the app, the e2e suite, and a fresh
-- `docker compose up` all have somewhere to put pages without a bootstrap
-- step. Multi-workspace provisioning arrives with permissions in M4.
INSERT INTO workspaces (id, name)
VALUES ('00000000-0000-0000-0000-000000000001', 'Default')
ON CONFLICT (id) DO NOTHING;
