-- Threaded comments, anchored to a position in the CRDT document.
--
-- `anchor` is an encoded `yrs::StickyIndex` and `quote` is the text it was
-- attached to. BOTH are needed: a sticky index alone CLAMPS to a surviving
-- position when its item is deleted, so a comment on a deleted sentence would
-- silently reattach to whatever text remains. The quote is what turns that into
-- an honest "orphaned".
--
-- A NULL anchor is a page-level comment — a legitimate thing, not a missing
-- value, which is why it is nullable rather than defaulted.
CREATE TABLE comments (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    page_id    uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    author_id  uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Replies point at their parent. A thread is one level deep by design:
    -- arbitrarily nested comment trees are hard to read and harder to anchor.
    parent_id  uuid REFERENCES comments(id) ON DELETE CASCADE,
    body       text NOT NULL,
    block_index int,
    anchor     bytea,
    quote      text,
    resolved   boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX comments_page_idx ON comments (page_id, created_at);

-- @-mentions, stored as rows rather than parsed out of the body on every read.
-- A mention is a fact ("this comment notifies Alice"), and re-deriving it from
-- prose means the notification list changes if the parser changes.
--
-- TWO tables, not one with two nullable columns. A single table keyed
-- (comment_id, user_id, page_id) cannot work: a user mention has no page, a
-- page mention has no user, and Postgres does not allow NULL in a PRIMARY KEY —
-- so every insert failed. They are also genuinely different facts: one
-- notifies a person, the other links a document.
CREATE TABLE comment_user_mentions (
    comment_id uuid NOT NULL REFERENCES comments(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (comment_id, user_id)
);
CREATE INDEX comment_user_mentions_user_idx ON comment_user_mentions (user_id);

CREATE TABLE comment_page_mentions (
    comment_id uuid NOT NULL REFERENCES comments(id) ON DELETE CASCADE,
    page_id    uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    PRIMARY KEY (comment_id, page_id)
);
CREATE INDEX comment_page_mentions_page_idx ON comment_page_mentions (page_id);
