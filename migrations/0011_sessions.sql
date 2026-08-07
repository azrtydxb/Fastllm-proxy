-- P4: admin session cookies (see
-- docs/superpowers/specs/2026-08-06-control-plane-rbac-routing-design.md,
-- "P4 -- Management UI"), and the fix for the gap `TODO.md` has carried since
-- P0: "/admin/* has no authentication of its own".
--
-- A session is the cookie-backed analogue of `api_keys`, deliberately kept
-- separate rather than folded into that table: an API key authenticates a
-- *caller* invoking a model and is checked on every inference request, a
-- session authenticates a *human* who passed an Argon2id password check at
-- login and is checked on every /admin/* request. Different verbs, different
-- lifetime (a session expires in hours, a key in months or never), different
-- revocation story (`DELETE /admin/keys/{id}` vs logout) -- conflating them
-- would make either one's schema wrong for the other.
--
-- `token_hash` follows `api_keys.hash`'s own precedent: the session token
-- handed to the browser as a cookie is high-entropy random (`rand`-generated,
-- same as an API key), so SHA-256 is the appropriate verifier here too --
-- unlike `principals.password_hash`, which is Argon2id specifically because a
-- human-chosen password is low-entropy and guessable. Don't unify the two.
CREATE TABLE sessions (
    id           BIGSERIAL PRIMARY KEY,
    token_hash   BYTEA NOT NULL UNIQUE,
    principal_id BIGINT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL
);
CREATE INDEX sessions_principal ON sessions(principal_id);
-- Every authenticated /admin/* request looks this up; an expired-but-unswept
-- row must not make that scan degrade over time.
CREATE INDEX sessions_expires_at ON sessions(expires_at);
