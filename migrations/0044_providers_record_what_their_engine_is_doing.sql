-- What the engine says about its own load, alongside whether it is up.
--
-- The sweep already dials every provider once a minute to ask what it serves.
-- The same pass can ask what it is busy with: vLLM and SGLang both publish
-- Prometheus at `/metrics`, so this is one text format rather than an endpoint
-- per vendor.
--
-- Why store it here rather than leave it to Prometheus. "1 of 1 up" is the
-- only thing the Providers screen can say today, and it is the same answer for
-- an idle box and one with forty requests queued. These three numbers are what
-- an operator actually wants when deciding where a model should live.
--
-- Nullable, all of them, and separately from `last_seen_at`: a provider that
-- answers `GET /v1/models` but publishes no metrics is healthy and unmeasured,
-- not degraded. Cloud providers are the normal case for that -- OpenRouter is
-- not going to show us its scheduler.

ALTER TABLE providers
    -- Requests the engine is working on now, across every engine in the
    -- server: a tensor-parallel deployment reports one sample per rank and the
    -- load that matters is the whole server's.
    ADD COLUMN engine_running  INTEGER,
    -- Accepted and not started. The number that says a backend is saturated
    -- rather than merely busy.
    ADD COLUMN engine_waiting  INTEGER,
    -- KV cache in use, 0..1. Averaged across ranks, because adding two
    -- half-full caches into one full one would be nonsense.
    ADD COLUMN engine_kv_cache REAL,
    -- When the numbers above were read. Without it a stale reading is
    -- indistinguishable from a current one, and routing must be able to tell
    -- the difference before it believes any of this.
    ADD COLUMN engine_load_at  TIMESTAMPTZ;

COMMENT ON COLUMN providers.engine_load_at IS
    'When engine_running/waiting/kv_cache were last read. NULL means this '
    'provider publishes no metrics, which is normal for a hosted one.';
