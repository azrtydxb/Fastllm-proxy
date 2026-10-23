-- A pool member that names one backend has to outlive that backend going away.
--
-- 0053 made `model_backend_id` ON DELETE CASCADE. The registrar deletes an
-- attachment when its host stops serving the model -- a reboot, a reload, the
-- provider swept after its grace window -- and creates a new one, with a new
-- id, when the host comes back. The cascade took the pool member with the
-- first half and nothing put it back in the second: after one reboot a pool
-- balancing across two Sparks balanced across one for good, and so did every
-- frontend model and rule pointing at that pool.
--
-- The answer rule targets got in 0036: bind by name, keep the id as a cache.
-- The name is the provider's, because `providers.name` is unique (`api_base`
-- is not) and a dynamic provider's name is the agent's, so it survives the
-- provider row being swept and registered again. `relink_targets` puts the id
-- back once that provider serves the model again.
--
-- Grain now reads off the name rather than the id. `backend_provider_name`
-- NULL is a model-grain member, carrying every provider of the model.
-- Non-NULL is a backend member: attached while `model_backend_id` is set,
-- waiting while it is not. A waiting member routes nowhere; it must never
-- widen into "every provider of the model", which is what reading grain off
-- a NULL id would make it.
ALTER TABLE model_pool_members ADD COLUMN backend_provider_name text;

UPDATE model_pool_members m
   SET backend_provider_name = p.name
  FROM model_backends mb
  JOIN providers p ON p.id = mb.provider_id
 WHERE mb.id = m.model_backend_id;

ALTER TABLE model_pool_members
    DROP CONSTRAINT model_pool_members_model_backend_id_fkey,
    ADD CONSTRAINT model_pool_members_model_backend_id_fkey
        FOREIGN KEY (model_backend_id) REFERENCES model_backends(id) ON DELETE SET NULL;

-- Uniqueness follows the binding. Keyed on the id, two members waiting at the
-- same time -- both hosts rebooted together -- would both fold to the
-- sentinel and the second detach would fail the registrar's delete.
DROP INDEX model_pool_members_pool_model_backend_key;
CREATE UNIQUE INDEX model_pool_members_pool_model_backend_key
    ON model_pool_members (pool_id, provider_model_id, COALESCE(backend_provider_name, ''));

COMMENT ON COLUMN model_pool_members.backend_provider_name IS
    'The provider this member routes to, by name: the binding that survives the '
    'attachment being deleted. NULL means the member is the model and brings '
    'every provider serving it.';
COMMENT ON COLUMN model_pool_members.model_backend_id IS
    'The attachment on backend_provider_name, cached. NULL with a provider name '
    'set means that provider does not serve the model right now; the member '
    'routes nowhere until relink_targets finds it again.';
