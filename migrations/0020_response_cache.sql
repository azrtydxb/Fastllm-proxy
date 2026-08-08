-- Per-model response caching, off unless asked for.
--
-- Caching changes semantics: two identical requests to a model at
-- temperature > 0 are supposed to be able to differ, and a gateway that
-- silently returns the first answer to both has changed what the caller asked
-- for. So this is opt-in per model rather than a global switch, and a
-- deployment that sets nothing pays nothing — not even the hash, which is only
-- computed once a model is known to have caching on.
--
-- Seconds. NULL or 0 means off, which is the default for every existing row.
ALTER TABLE models
    ADD COLUMN cache_ttl_seconds INTEGER NULL
        CHECK (cache_ttl_seconds IS NULL OR cache_ttl_seconds >= 0);
