-- P1: virtual models and rule-based routing (see
-- docs/superpowers/specs/2026-08-06-control-plane-rbac-routing-design.md,
-- "P1 -- Virtual models and routing rules").
--
-- A virtual model is a client-facing name with an ordered list of rules; the
-- first rule whose conditions match supplies the targets, and if none match
-- the virtual model's own defaults apply. `crate::control::build` resolves
-- all four tables below into `crate::routing::VirtualModelDef` once per
-- snapshot rebuild, exactly like RBAC grants are pre-flattened -- the
-- request path never queries any of this.
CREATE TABLE virtual_models (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT ''
);

-- `match_json` holds a `crate::routing::MatchConditionJson`: which caller
-- (principal id or role name) and which approximate request shape
-- (estimated prompt size, requested max_tokens) this rule applies to, with
-- every field optional and absence meaning "no constraint on this axis".
-- JSONB rather than a wider table of nullable columns because the match
-- shape is still settling (see the design doc's own hedging on exactly
-- which conditions matter) and a new condition should not need a migration
-- to add a column for.
--
-- `position` orders the rule chain within one virtual model; evaluation
-- stops at the first match, so position is load-bearing, not cosmetic.
CREATE TABLE routing_rules (
    id               BIGSERIAL PRIMARY KEY,
    virtual_model_id BIGINT NOT NULL REFERENCES virtual_models(id) ON DELETE CASCADE,
    position         INT NOT NULL,
    match_json       JSONB NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (virtual_model_id, position)
);
CREATE INDEX routing_rules_virtual_model ON routing_rules(virtual_model_id);

-- A rule's ordered chain of targets. `model_id` references `models`, not
-- `virtual_models` -- this foreign key is what makes "a virtual model
-- targets concrete models only" a schema-level fact rather than a runtime
-- check: a virtual model targeting another virtual model cannot be expressed
-- here at all, so there is no cycle to detect and no evaluation-depth limit
-- to enforce.
--
-- `weight` is a relative share for the deterministic weighted split
-- (canary/A-B; see `crate::routing::choose_weighted`), not a percentage --
-- targets weighted 1 and 3 split 25/75 without needing to sum to 100.
-- `position` is the failover order used when the weighted pick's target pool
-- turns out to be unhealthy or saturated (`crate::routing::VirtualModelDef::resolve`).
CREATE TABLE rule_targets (
    id       BIGSERIAL PRIMARY KEY,
    rule_id  BIGINT NOT NULL REFERENCES routing_rules(id) ON DELETE CASCADE,
    model_id BIGINT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    weight   INT NOT NULL DEFAULT 100,
    position INT NOT NULL,
    UNIQUE (rule_id, position)
);
CREATE INDEX rule_targets_rule ON rule_targets(rule_id);

-- A virtual model's fallback targets, used when no rule matches. Not shaped
-- as "a rule with an always-true condition" on purpose: that would make
-- "no rule matched" indistinguishable from "the last rule matched and its
-- condition happened to be empty", and the design draws that line
-- explicitly ("if none match, the virtual model's defaults apply"). Same
-- weight/position semantics as `rule_targets`, and the same concrete-only
-- foreign key.
CREATE TABLE virtual_model_defaults (
    id               BIGSERIAL PRIMARY KEY,
    virtual_model_id BIGINT NOT NULL REFERENCES virtual_models(id) ON DELETE CASCADE,
    model_id         BIGINT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    weight           INT NOT NULL DEFAULT 100,
    position         INT NOT NULL,
    UNIQUE (virtual_model_id, position)
);
CREATE INDEX virtual_model_defaults_vm ON virtual_model_defaults(virtual_model_id);
