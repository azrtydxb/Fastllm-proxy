-- Retention for `usage_events`, which now gets a row per request.
--
-- Before the accounting change this table grew only when a principal had a
-- budget, so it barely grew at all -- nineteen rows in five days. It now
-- takes one row per request forever, which is a table that needs a policy
-- rather than a table that needs watching.
--
-- The policy: raw rows for 90 days, hourly rollups beyond that, kept
-- indefinitely. Raw is what per-request questions need ("which caller got
-- slow last Tuesday"), and those questions have a short shelf life. The
-- long-lived question is shape over time -- how much, by whom, costing what
-- -- and an hourly bucket answers that at a thousandth of the size.
--
-- WHAT THIS TABLE DELIBERATELY DOES NOT CARRY: latency percentiles.
-- Percentiles do not merge. Averaging two hours' p95 gives a number that is
-- not the p95 of anything, and storing it would make a chart that looks
-- continuous across the 90-day boundary while quietly changing what it
-- means. Rolled-up buckets report no latency at all, and the chart breaks
-- its line there -- the same treatment an empty bucket gets, for the same
-- reason: unknown is not zero and not an average.
--
-- Sums are kept instead, because sums do merge. A mean duration is
-- recoverable from `duration_ms_sum / duration_ms_count` by anyone who wants
-- one, without this table pretending it is a percentile.
CREATE TABLE usage_rollup_hourly (
    hour                  timestamptz NOT NULL,
    model_id              bigint,
    principal_id          bigint,
    requests              bigint NOT NULL,
    upstream_errors       bigint NOT NULL,
    refused_authorisation bigint NOT NULL,
    refused_rate_limit    bigint NOT NULL,
    refused_budget        bigint NOT NULL,
    refused_no_backend    bigint NOT NULL,
    prompt_tokens         bigint NOT NULL,
    completion_tokens     bigint NOT NULL,
    cost_micros           bigint NOT NULL,
    unpriced_requests     bigint NOT NULL,
    duration_ms_sum       bigint NOT NULL,
    duration_ms_count     bigint NOT NULL,
    PRIMARY KEY (hour, model_id, principal_id)
);

-- `model_id` and `principal_id` are nullable here although they are not in
-- `usage_events`, and the primary key above tolerates it, because a rollup
-- outlives the rows it summarises: a model deleted next year must not take
-- last year's spend with it. `usage_events` has no such foreign key either,
-- for the same reason -- see its ingest join, which drops unresolvable rows
-- rather than cascading.

CREATE INDEX usage_rollup_hour ON usage_rollup_hourly (hour DESC);
