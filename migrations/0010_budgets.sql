-- P3: usage accounting and budgets (see
-- docs/superpowers/specs/2026-08-06-control-plane-rbac-routing-design.md,
-- "P3 -- Usage accounting and budgets").
--
-- One row per principal that has a configured budget. A principal with no
-- row here is unlimited, not zero -- the same "absence, not a bucket with
-- capacity zero" convention `limits` (migrations/0009) already established;
-- `crate::control::build::build_snapshot` turns "no row" into
-- `Principal.budget = None`, and the request path's enforcement
-- (`crate::proxy`) never even reaches a comparison for a principal nobody
-- configured a budget for.
--
-- `tokens_used` is the running counter the snapshot carries verbatim (see
-- the comment on `usage_events` in migrations/0005) -- incremented by
-- `crate::control::api::post_usage` as batches of real, upstream-reported
-- token counts arrive, and reset to zero on window rollover (see `window`
-- below and `crate::control::build`'s rollover logic, run on every snapshot
-- rebuild).
CREATE TABLE budgets (
    principal_id  BIGINT PRIMARY KEY REFERENCES principals(id) ON DELETE CASCADE,
    tokens_total  BIGINT NOT NULL CHECK (tokens_total > 0),
    tokens_used   BIGINT NOT NULL DEFAULT 0 CHECK (tokens_used >= 0),
    window_start  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Named `budget_window`, not `window` -- the latter is a reserved word
    -- in Postgres (window functions) and would need quoting on every single
    -- reference. Fixed-length, not calendar arithmetic: 'monthly' is a
    -- rolling 30-day period, not "the 1st of next month" -- see
    -- `crate::control::build::BudgetWindow` for why that simplification was
    -- made and what it costs.
    budget_window TEXT NOT NULL CHECK (budget_window IN ('daily', 'weekly', 'monthly')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
