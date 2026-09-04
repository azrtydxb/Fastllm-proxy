-- Usage stops being deleted along with the model it names.
--
-- `usage_events.model_id` was `ON DELETE CASCADE`, and 0005 argued for it:
-- "a usage row for a principal or model that no longer exists describes
-- nothing an operator can act on, so it is not worth keeping around as an
-- orphan." That held while models were deleted rarely and deliberately, by a
-- human. It stops holding the moment anything deletes them on a schedule --
-- a registration service expiring a lease, or a model swapped off a host --
-- which is the whole point of the work this migration belongs to.
--
-- The consequence was silent: swap a model off a host on Friday and that
-- week's inference disappears from usage and spend, with no error, because
-- the cascade is doing what it was told.
--
-- Recording the name as text fixes something the id alone never could,
-- either. Names get reused -- 0029 already renames models, and a provider
-- that stops serving `qwen3.8-27b` and later serves it again produces a
-- second, different row under one name -- so an id-only record of what was
-- billed is ambiguous before anything is deleted at all.

ALTER TABLE usage_events
  -- What the model was called when the request ran, and where it ran. Written
  -- at ingest and never updated: what a request cost, and what served it, are
  -- facts about when it happened, the same reasoning `cost_micros` already
  -- follows by storing the price rather than deriving it on read.
  ADD COLUMN model_name    TEXT,
  ADD COLUMN provider_name TEXT;

-- Backfill from what is currently true. It is the best available answer for
-- rows written before this column existed, and it is exactly right for every
-- row whose model has not been renamed since.
UPDATE usage_events u
SET model_name = m.name,
    provider_name = p.name
FROM models m
LEFT JOIN providers p ON p.id = m.provider_id
WHERE m.id = u.model_id;

-- The id stays, as a convenience for joining to a model that still exists,
-- but it stops being the thing that keeps the row alive.
ALTER TABLE usage_events ALTER COLUMN model_id DROP NOT NULL;
ALTER TABLE usage_events DROP CONSTRAINT usage_events_model_id_fkey;
ALTER TABLE usage_events
  ADD CONSTRAINT usage_events_model_id_fkey
  FOREIGN KEY (model_id) REFERENCES models(id) ON DELETE SET NULL;

COMMENT ON COLUMN usage_events.model_id IS
    'The model that served this request, while it still exists. NULL once it '
    'has been deleted -- read model_name instead, which is what the request '
    'was actually billed under.';

-- Reporting groups by name now, so the index that served `GROUP BY model_id`
-- needs a sibling. The old one is kept: joins to `models` still use the id.
CREATE INDEX usage_events_model_name ON usage_events(model_name);

-- The hourly rollup has the same problem, and a sharper version of it.
--
-- `usage_rollup_hourly.model_id` is `NOT NULL` and part of the primary key, so
-- once `usage_events.model_id` can be NULL the rollup does not merely lose a
-- name -- the INSERT fails, and the whole batch with it. Retention would stop
-- working the first time it met a request served by a since-deleted model.
--
-- So the rollup is keyed by the name too. That is the better key regardless:
-- it is what the request was billed under, it survives the model, and it does
-- not silently merge two different models that happened to reuse an id.
ALTER TABLE usage_rollup_hourly ADD COLUMN model_name TEXT;

UPDATE usage_rollup_hourly r SET model_name = m.name
FROM models m WHERE m.id = r.model_id;

-- Any row whose model has already gone keeps a placeholder rather than being
-- dropped: the totals are real even when the name is no longer recoverable.
UPDATE usage_rollup_hourly SET model_name = '(deleted model ' || model_id || ')'
WHERE model_name IS NULL;

ALTER TABLE usage_rollup_hourly ALTER COLUMN model_name SET NOT NULL;
-- The primary key goes first: Postgres refuses to drop NOT NULL from a column
-- while it is part of one.
ALTER TABLE usage_rollup_hourly DROP CONSTRAINT usage_rollup_hourly_pkey;
ALTER TABLE usage_rollup_hourly ALTER COLUMN model_id DROP NOT NULL;
ALTER TABLE usage_rollup_hourly
  ADD CONSTRAINT usage_rollup_hourly_pkey PRIMARY KEY (hour, model_name, principal_id);
