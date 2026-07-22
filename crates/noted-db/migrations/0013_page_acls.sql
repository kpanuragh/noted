-- Per-page access, on top of workspace membership.
--
-- Membership (0012) answers "may you see this workspace at all". This answers
-- "may you see THIS page" — the finer question a shared team workspace needs,
-- where a salary review and a lunch menu live side by side.
--
-- INHERITANCE IS THE DEFAULT, and absence of a row means "same as my parent".
-- Storing a row per page per user would mean writing thousands of rows when
-- someone shares a subtree, and re-writing them all when a page moves. So a row
-- here is an OVERRIDE — an explicit grant or an explicit denial at one point in
-- the tree — and the effective answer is the nearest override walking up to the
-- root, falling back to workspace membership when there is none.
CREATE TABLE page_acls (
    page_id  uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    user_id  uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- 'read' | 'write' | 'none'. 'none' is a DENIAL and is the reason this is
    -- not simply a grant table: revoking one person's access to one subtree of
    -- a workspace they otherwise belong to has no other representation.
    access   text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (page_id, user_id)
);

CREATE INDEX page_acls_user_idx ON page_acls (user_id);
