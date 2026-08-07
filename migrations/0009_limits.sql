-- P2: rate limits with reconciliation (see
-- docs/superpowers/specs/2026-08-06-control-plane-rbac-routing-design.md,
-- "P2 -- Rate limiting").
--
-- One row per principal that has a configured limit. A principal with no row
-- here is unlimited on both dimensions, not zero -- `crate::control::build`
-- turns "no row" into `Principal.limits = None`, and the request path's
-- `crate::limiter::Limiter::check` short-circuits to admitting immediately
-- whenever `Limits::is_unlimited()`, never allocating a bucket for a
-- principal nobody configured one for. Getting that direction backwards
-- would deny every request from every principal with no configured limit,
-- which is most of them.
CREATE TABLE limits (
    principal_id     BIGINT PRIMARY KEY REFERENCES principals(id) ON DELETE CASCADE,
    -- Either column may be set on its own: the two dimensions are enforced
    -- independently by `crate::limiter::Limits`, so a principal can have
    -- only a requests/min cap, only a tokens/min cap, or both.
    requests_per_min INT,
    tokens_per_min   INT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (requests_per_min IS NOT NULL OR tokens_per_min IS NOT NULL),
    CHECK (requests_per_min IS NULL OR requests_per_min > 0),
    CHECK (tokens_per_min IS NULL OR tokens_per_min > 0)
);
