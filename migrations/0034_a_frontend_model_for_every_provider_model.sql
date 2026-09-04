-- Give every provider model a frontend model in front of it.
--
-- Authorisation is moving to the frontend model, which means frontend models
-- become the only name a client can use (ADR 0002). Anything a caller names
-- directly today that has no frontend model would become a silent 404 the
-- moment that lands. On the dev cluster that was three -- `gpt-5`,
-- `gemini-2.5-flash` and `openrouter-free` -- and `gpt-5` is not hypothetical:
-- the request that verified the provider decomposition end to end named it.
--
-- So this runs first, on its own, while both names still resolve. It changes
-- nothing for any caller: a frontend model shadows a provider model of the
-- same name during resolution (`resolve_target_models` looks in
-- `frontend_models` first), and the frontend model created here routes
-- straight back to the provider model that was being reached before. The
-- request takes one more lookup and lands in the same place.
--
-- Deliberately *not* renaming the provider model out of the way, the way
-- migration 0029 did when it split one. Renaming revokes every grant naming
-- it -- 0029 proved that in production, and 0030 exists to clean it up. The
-- shadowing is benign precisely because the frontend model's only target is
-- the model it shadows.

INSERT INTO frontend_models (name, description)
SELECT pm.name,
       'Created by migration 0034 so ' || pm.name || ' stays callable when '
       || 'frontend models become the only addressable surface.'
FROM provider_models pm
WHERE NOT EXISTS (
        SELECT 1 FROM frontend_model_defaults d
         WHERE d.provider_model_id = pm.id)
  AND NOT EXISTS (
        SELECT 1 FROM rule_targets rt
         WHERE rt.provider_model_id = pm.id)
  -- A name already taken by a frontend model is already covered by it.
  AND NOT EXISTS (
        SELECT 1 FROM frontend_models fm WHERE fm.name = pm.name);

INSERT INTO frontend_model_defaults (frontend_model_id, provider_model_id, weight, position)
SELECT fm.id, pm.id, 1, 0
FROM frontend_models fm
JOIN provider_models pm ON pm.name = fm.name
WHERE NOT EXISTS (
        SELECT 1 FROM frontend_model_defaults d
         WHERE d.frontend_model_id = fm.id);
