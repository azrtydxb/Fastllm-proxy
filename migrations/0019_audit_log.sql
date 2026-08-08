-- Who changed the configuration, and to what.
--
-- `usage_events` records inference: which principal called which model. It
-- says nothing about the other kind of action — who created a key, granted a
-- role, raised a budget, repointed a backend at a different provider. Those
-- are the changes an incident review asks about, and until now the only
-- evidence was whatever happened to be in a log file.
--
-- Deliberately append-only in practice: nothing in this codebase updates or
-- deletes a row here, and `ON DELETE SET NULL` on the actor is what keeps that
-- true when a principal is removed. A trail that disappears with the account
-- that did the thing is not a trail.
CREATE TABLE audit_events (
    id          BIGSERIAL PRIMARY KEY,

    -- The principal whose session made the change. NULL once that principal is
    -- deleted, and NULL for a change made by a proxy token rather than a human
    -- session; `actor_name` keeps the answer readable either way.
    actor_id    BIGINT NULL REFERENCES principals(id) ON DELETE SET NULL,
    actor_name  TEXT NOT NULL,

    -- What was done, as a stable machine-readable verb: `key.create`,
    -- `role.grant`, `backend.delete`. Not a sentence — sentences get reworded
    -- and break every query written against them.
    action      TEXT NOT NULL,
    -- What it was done to, e.g. `principal:42` or `model:qwen3`.
    target      TEXT NOT NULL,

    -- Anything worth keeping that is not the verb or the target: the fields
    -- that changed, the rule that was written. Never the secret itself — an
    -- audit row is read by more people than the thing it describes.
    detail      JSONB NOT NULL DEFAULT '{}'::jsonb,

    at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The two questions actually asked of this table: "what happened recently" and
-- "what has this person been doing".
CREATE INDEX audit_events_at ON audit_events (at DESC);
CREATE INDEX audit_events_actor ON audit_events (actor_id, at DESC);
