-- A usage row now exists for every attributable request, not only for the
-- ones whose response carried a usage block.
--
-- Why this column has to exist rather than letting zeroes stand in: the two
-- facts are different and only one of them is about tokens. A request that
-- genuinely consumed nothing and a request whose token counts were never
-- reported both arrive here as `prompt_tokens = 0, completion_tokens = 0`,
-- and summing them together silently understates consumption. The old
-- behaviour avoided the ambiguity by not writing the row at all, which
-- traded one wrong number for a missing one: an upstream 502 carries no
-- usage block, so the errors were exactly the rows that never appeared, and
-- no error rate could be computed from this table at all.
--
-- DEFAULT TRUE backfills correctly and is not a guess. Every row that
-- existed before this migration was written under the old rule, which only
-- recorded when real counts had been read off the response — so for all of
-- them, the counts were reported.
ALTER TABLE usage_events
    ADD COLUMN usage_reported boolean NOT NULL DEFAULT true;

-- Charts scan a time range and group by bucket, which the existing
-- `usage_events_at` index already serves. These two are for the filtered
-- forms the drill-down offers — "this model, over this window" and "errors
-- only, over this window" — where scanning the whole range and discarding
-- most of it is the difference between a modal that opens and one that
-- hangs.
--
-- `status >= 400` as a partial index rather than an index on `status`:
-- errors are the small minority of rows and the only value anyone filters
-- on, so indexing the common case would be paying to find what is already
-- almost everything.
CREATE INDEX usage_events_model_at ON usage_events (model_id, at DESC);
CREATE INDEX usage_events_errors_at ON usage_events (at DESC) WHERE status >= 400;
