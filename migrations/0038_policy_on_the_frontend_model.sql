-- The load-balancing policy moves to where the things being balanced are.
--
-- Migration 0028 put `policy` on the model, and said why: "how to choose
-- between one backend model's replicas". After the provider split a provider
-- model has exactly one provider and therefore one backend, so its pool has
-- one member and the policy chooses between one thing. It has been dead since
-- 0029 without anyone noticing, which is what a setting that silently does
-- nothing looks like.
--
-- The things that actually need choosing between are a frontend model's
-- targets, and until now the only way to express a preference among them was
-- a weight. That is one policy -- a deterministic split on the request prefix
-- -- and 0028's own example needs the others: two local replicas sharing a
-- prefix cache want affinity, three hosted providers of differing speed want
-- lowest latency, and one deployment commonly has both.

ALTER TABLE frontend_models ADD COLUMN policy TEXT;

-- Carried from the targets, where a value survives from before the split.
-- Taking the first target's is not arbitrary: every target of one frontend
-- model came from one pool when 0029 split it, so they agree.
UPDATE frontend_models fm
SET policy = pm.policy
FROM frontend_model_defaults d
JOIN provider_models pm ON pm.id = d.provider_model_id
WHERE d.frontend_model_id = fm.id
  AND pm.policy IS NOT NULL
  AND d.position = 0;

-- Gone from the provider model, rather than left to read as though it still
-- governs something. A column that no longer has replicas to choose between
-- is worse than absent: it appears on the screen and in the API as a knob.
ALTER TABLE provider_models DROP COLUMN policy;

COMMENT ON COLUMN frontend_models.policy IS
    'How to choose between this frontend model''s targets. NULL means the '
    'weighted split, which is what a target list has always meant and stays '
    'the default.';
