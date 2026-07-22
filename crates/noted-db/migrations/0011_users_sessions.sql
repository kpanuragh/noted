-- Identity. Until this migration every request was trusted and the product had
-- exactly one workspace, seeded by 0001.
--
-- `email` is CITEXT-like by convention rather than by type: it is stored as
-- given and compared lowercased by the repository, so "A@b.com" and "a@b.com"
-- cannot become two accounts. A UNIQUE index on the lowercased value enforces
-- that at the database rather than trusting every call site to remember.
CREATE TABLE users (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email         text NOT NULL,
    password_hash text NOT NULL,
    display_name  text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX users_email_lower_idx ON users (lower(email));

-- Sessions store the SHA-256 OF THE TOKEN, never the token.
--
-- A stolen database dump therefore does not hand the attacker live sessions:
-- the column is a one-way function of a value only the browser holds. This is
-- the same reasoning as password hashing, applied to the credential that is
-- actually presented on every request.
--
-- `expires_at` is enforced in the lookup query, not by a sweeper. A sweeper that
-- falls behind would leave sessions live past their expiry; a predicate cannot.
-- Deleting expired rows is housekeeping, not security.
CREATE TABLE sessions (
    token_hash text PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL
);
CREATE INDEX sessions_user_idx ON sessions (user_id);
CREATE INDEX sessions_expires_idx ON sessions (expires_at);
