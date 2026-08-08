-- Latency and outcome alongside the token counts.
--
-- Metrics answer "did the 99th percentile move". They cannot answer "for whom",
-- because the answer is per caller and per key, and a Prometheus label with
-- that cardinality is how a metrics endpoint becomes an outage. This is where
-- that detail belongs instead: already batched off the request path, already
-- keyed by principal, and in a database where high cardinality is ordinary.
--
-- Nullable rather than defaulted. A row written by a proxy that predates this
-- column, or one whose response ended before a first byte was ever sent, has no
-- honest number to put here — and a zero would be indistinguishable from a
-- request that answered instantly.
ALTER TABLE usage_events
    ADD COLUMN duration_ms INTEGER NULL,
    -- Time to first token. NULL for a non-streaming response, where it would
    -- be a copy of duration_ms rather than a second measurement.
    ADD COLUMN ttft_ms     INTEGER NULL,
    -- The upstream's HTTP status. NULL when no upstream was reached at all.
    ADD COLUMN status      SMALLINT NULL,
    -- What the client asked for, when that differs from the model that served
    -- it — a virtual model name, or the head of a chain that failed over. The
    -- existing model_id is always the model that actually answered.
    ADD COLUMN requested_model TEXT NULL;

-- Answering "which callers got slow, and when" without scanning the table.
CREATE INDEX usage_events_principal_at ON usage_events (principal_id, at DESC);
