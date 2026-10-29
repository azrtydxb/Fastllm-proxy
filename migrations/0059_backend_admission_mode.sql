-- A NULL concurrency is ambiguous: it was used for both an unset setting
-- and an intentional opt-out. Keep the choice explicit for future defaults.
ALTER TABLE model_backends
    ADD COLUMN admission_mode TEXT NOT NULL DEFAULT 'auto'
        CHECK (admission_mode IN ('auto', 'explicit', 'off'));

-- Existing configured gates retain their meaning; NULL remains automatic.
UPDATE model_backends
SET admission_mode = CASE WHEN admission_max_concurrent IS NULL THEN 'auto' ELSE 'explicit' END;
