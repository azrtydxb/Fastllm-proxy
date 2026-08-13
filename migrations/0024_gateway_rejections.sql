-- Refusals the gateway makes before it knows who is calling.
--
-- `usage_events` holds one row per attributable request, including the
-- refusals the gateway decides itself -- 403, 429, 402, and the 502 for an
-- unreachable chain. What it cannot hold is a refusal with no principal to
-- attribute: a 401 from an invalid key, and a 404 for a model that does not
-- exist. Both are caller-visible failures, and their absence meant an error
-- rate drawn from Postgres alone was really a success-plus-some-failures
-- rate wearing the wrong name.
--
-- They are not per-request rows here, and that is the point. 401 is the one
-- refusal an anonymous stranger can trigger at will, so a row per occurrence
-- would let unauthenticated traffic drive unbounded writes into the
-- database. These are counts, bucketed to the minute and keyed by replica:
-- the write rate is bounded by the number of replicas and the health-report
-- interval, not by how hard anyone is knocking on the door.
--
-- The counters arrive on the health report as cumulative per-process values,
-- and the control plane stores deltas -- it holds each replica's previous
-- report already, so the subtraction happens where the state is. A counter
-- that went *down* means the replica restarted, and the new value is taken
-- as the delta rather than producing a negative.
CREATE TABLE gateway_rejections (
    at       timestamptz NOT NULL,
    replica  text        NOT NULL,
    kind     text        NOT NULL,
    count    bigint      NOT NULL,
    PRIMARY KEY (at, replica, kind)
);

CREATE INDEX gateway_rejections_at ON gateway_rejections (at DESC);
