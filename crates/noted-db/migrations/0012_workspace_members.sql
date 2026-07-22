-- Who may name a given workspace_id.
--
-- Every query in this codebase is already workspace-scoped — that was hard-won,
-- and it means the graph, the chunks and the embeddings are partitioned by
-- tenant by construction. What was missing is the other half: nothing checked
-- that the caller was ENTITLED to the workspace_id they passed. A signed-in
-- user could read any workspace simply by knowing its uuid.
--
-- `role` is a text column rather than an enum: roles will grow (viewer,
-- commenter, guest) and an enum makes each addition a migration. The set is
-- validated in the repository, which is also where the permission logic that
-- reads it lives.
CREATE TABLE workspace_members (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users(id)      ON DELETE CASCADE,
    role         text NOT NULL DEFAULT 'member',
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);

-- The membership check runs on EVERY request that names a workspace, so it must
-- be an index lookup and never a scan. The PK covers (workspace_id, user_id);
-- this covers the other direction — "which workspaces does this user have?" —
-- which is what the workspace switcher asks on every page load.
CREATE INDEX workspace_members_user_idx ON workspace_members (user_id);
