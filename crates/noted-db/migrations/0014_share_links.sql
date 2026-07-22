-- Public, tokenised access to one page — the "send someone a link" surface.
--
-- Stores the SHA-256 OF THE TOKEN, never the token, for the same reason
-- `sessions` does (0011): a stolen database dump must not hand over live links.
-- The token exists only in the URL the sharer copied.
--
-- `include_descendants` is a property of the LINK, not of the page. Sharing a
-- design doc usually means sharing its sub-pages; sharing one meeting note
-- usually does not. Making it per-link means the same page can be shared both
-- ways to different people without either choice overwriting the other.
--
-- `expires_at` is NULL for "no expiry". Enforced in the lookup query rather than
-- by a sweeper, exactly as session expiry is: a sweeper that falls behind leaves
-- dead links working, and a predicate cannot fall behind.
CREATE TABLE share_links (
    token_hash          text PRIMARY KEY,
    page_id             uuid NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    created_by          uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    include_descendants boolean NOT NULL DEFAULT false,
    expires_at          timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now()
);

-- "Which links exist for this page" — what the share dialog asks every time it
-- opens, and what revocation-by-page needs.
CREATE INDEX share_links_page_idx ON share_links (page_id);
