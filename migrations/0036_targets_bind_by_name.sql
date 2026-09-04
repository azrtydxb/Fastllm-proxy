-- A frontend model's targets stop being erased when a provider model is.
--
-- `frontend_model_defaults.provider_model_id` and `rule_targets.provider_model_id`
-- are `ON DELETE CASCADE`. That was fine while provider models were deleted
-- rarely and by hand. Once a registration service deletes them on a lease, a
-- frontend model quietly loses a target every time a host is decommissioned or
-- a model is swapped -- and loses it *completely*: the row is gone, so nothing
-- records what it used to point at, and re-registering the same model does not
-- bring the routing back. Someone has to notice and re-add it.
--
-- Migration 0032 is the precedent. It repaired frontend models whose targets
-- had been silently repointed by 0029, and could only do so because the models
-- still existed under a derivable name. A delete leaves nothing to derive from.
--
-- So the target records the name it wants -- provider and model -- and keeps
-- it. Deleting the provider model clears the id and leaves the row: not
-- routable, visible as such, and reattached by itself when a model of that
-- name shows up on that provider again. Which, for a host that swaps a model
-- out and back, is the normal case rather than the exception.

ALTER TABLE frontend_model_defaults
  ADD COLUMN target_provider_name TEXT,
  ADD COLUMN target_model_name    TEXT;
ALTER TABLE rule_targets
  ADD COLUMN target_provider_name TEXT,
  ADD COLUMN target_model_name    TEXT;

-- Backfill from what each target points at now.
UPDATE frontend_model_defaults d
SET target_model_name    = pm.name,
    target_provider_name = p.name
FROM provider_models pm
LEFT JOIN providers p ON p.id = pm.provider_id
WHERE pm.id = d.provider_model_id;

UPDATE rule_targets rt
SET target_model_name    = pm.name,
    target_provider_name = p.name
FROM provider_models pm
LEFT JOIN providers p ON p.id = pm.provider_id
WHERE pm.id = rt.provider_model_id;

-- Every existing row has a target, so the name is required from here on: a
-- target that names nothing could never be reattached, which is the whole
-- point of the column.
ALTER TABLE frontend_model_defaults ALTER COLUMN target_model_name SET NOT NULL;
ALTER TABLE rule_targets            ALTER COLUMN target_model_name SET NOT NULL;

-- The id becomes a cache of "which row is that name right now", not the
-- binding itself.
ALTER TABLE frontend_model_defaults ALTER COLUMN provider_model_id DROP NOT NULL;
-- Constraint names survive a table rename, so this is still the name
-- migration 0001 gave it on `virtual_model_defaults`.
ALTER TABLE frontend_model_defaults DROP CONSTRAINT virtual_model_defaults_model_id_fkey;
ALTER TABLE frontend_model_defaults
  ADD CONSTRAINT frontend_model_defaults_provider_model_id_fkey
  FOREIGN KEY (provider_model_id) REFERENCES provider_models(id) ON DELETE SET NULL;

ALTER TABLE rule_targets ALTER COLUMN provider_model_id DROP NOT NULL;
ALTER TABLE rule_targets DROP CONSTRAINT rule_targets_model_id_fkey;
ALTER TABLE rule_targets
  ADD CONSTRAINT rule_targets_provider_model_id_fkey
  FOREIGN KEY (provider_model_id) REFERENCES provider_models(id) ON DELETE SET NULL;

COMMENT ON COLUMN frontend_model_defaults.target_model_name IS
    'The provider model this target wants, by name. Survives that model being '
    'deleted, so re-registering it reattaches routing with no manual step.';
COMMENT ON COLUMN rule_targets.target_model_name IS
    'The provider model this target wants, by name. See the sibling column on '
    'frontend_model_defaults.';
