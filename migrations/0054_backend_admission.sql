-- Per-backend admission control: bounded queueing in front of an engine.
--
-- vLLM accepts every request and queues the surplus itself, unbounded, so a
-- burst makes every request slow instead of failing any. With a gate, each
-- proxy holds the surplus instead -- bounded, and refused with 503 and
-- Retry-After once it is full -- and lets through only as many as the engine
-- is keeping up with, judged from the queue depth the engine reports.
--
-- Per backend rather than per deployment because the right numbers are a
-- property of the engine: a 35B on one GPU and an embedding model on the same
-- box want nothing like the same ceiling, and a hosted provider wants none.
--
-- `admission_max_concurrent` NULL is off, which is every existing row. The
-- other three are tuning with defaults that are sane for a single-GPU vLLM,
-- so turning a gate on is one number.
ALTER TABLE model_backends
    ADD COLUMN admission_max_concurrent   INTEGER DEFAULT NULL,
    ADD COLUMN admission_high_water       INTEGER NOT NULL DEFAULT 4,
    ADD COLUMN admission_max_queued       INTEGER NOT NULL DEFAULT 64,
    ADD COLUMN admission_max_wait_seconds INTEGER NOT NULL DEFAULT 30;

ALTER TABLE model_backends
    ADD CONSTRAINT admission_max_concurrent_check
        CHECK (admission_max_concurrent IS NULL OR admission_max_concurrent >= 1),
    ADD CONSTRAINT admission_high_water_check CHECK (admission_high_water >= 1),
    ADD CONSTRAINT admission_max_queued_check CHECK (admission_max_queued >= 0),
    ADD CONSTRAINT admission_max_wait_seconds_check CHECK (admission_max_wait_seconds >= 0);
