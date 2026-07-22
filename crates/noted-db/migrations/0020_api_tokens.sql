-- API tokens for programmatic access.
--
-- Stores the SHA-256 of the token, never the token — the third time this
-- pattern appears (sessions 0011, share links 0014), and for the same reason: a
-- stolen database dump must not hand over live credentials.
--
-- `scopes` is a text array rather than a bitmask or a role name. A bitmask is
-- unreadable in a database dump when you are trying to work out what a leaked
-- token could do, and a role name means adding a capability edits every role.
CREATE TABLE api_tokens (
    token_hash   text PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name         text NOT NULL,
    scopes       text[] NOT NULL DEFAULT '{}',
    expires_at   timestamptz,
    last_used_at timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX api_tokens_user_idx ON api_tokens (user_id);
CREATE INDEX api_tokens_workspace_idx ON api_tokens (workspace_id);
