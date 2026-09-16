-- Add per-backend upstream timeout override.
-- When set, this backend may take longer for first-byte than the global
-- --upstream-timeout, which is important for self-hosted long-context
-- engines that legitimately need minutes for a 100k-token prefill.
-- The proxy uses this value if present, falling back to the global flag.

ALTER TABLE model_backends ADD COLUMN upstream_timeout_seconds INTEGER DEFAULT NULL;
ALTER TABLE model_backends ADD CONSTRAINT upstream_timeout_seconds_check CHECK (upstream_timeout_seconds IS NULL OR upstream_timeout_seconds >= 1);
