-- Two knobs that were one, and neither was where it had to be.
--
-- Routing happens twice, and the two levels answer different questions.
-- Choosing between *targets* -- qwen locally or gemini at OpenRouter -- is a
-- policy decision about cost and capability. Choosing between the *backends*
-- of one model is a mechanical one about which of several interchangeable
-- machines is warmest or least busy. Until migration 0045 the second level
-- had nothing to choose between, so the distinction did not bite.
--
-- `frontend_models.policy` set the first level for a whole frontend model, so
-- "least-connections across the two Sparks, then plain failover to the cloud
-- when they are full" could not be written: the two rules had to share one
-- policy. It moves onto the rule, where the targets it applies to are.
-- `frontend_models.policy` stays and now means only what the *default*
-- targets use, which is the one target list that has no rule of its own.
ALTER TABLE routing_rules
    ADD COLUMN policy text
        CHECK (policy IS NULL OR policy IN ('cache-affinity', 'least-loaded',
                                            'round-robin', 'lowest-latency', 'cheapest'));

-- And the second level becomes settable at all. A model's backends may be two
-- identical local replicas sharing a prefix cache, or one model offered by
-- three vendors at different prices; those want different answers, and one
-- deployment routinely holds both. NULL means the deployment's `--policy`.
ALTER TABLE provider_models
    ADD COLUMN policy text
        CHECK (policy IS NULL OR policy IN ('cache-affinity', 'least-loaded',
                                            'round-robin', 'lowest-latency', 'cheapest'));
