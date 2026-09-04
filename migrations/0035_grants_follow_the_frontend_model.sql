-- Grant on the frontend model what is currently granted on all of its targets.
--
-- Authorisation is about to check the name the caller actually used. A caller
-- naming a frontend model is authorised against that frontend model, not
-- against whichever provider model its rules happen to resolve to -- which is
-- what "we do RBAC on frontend models, this is how we expose them" means, and
-- what stops a provider model being renamed or replaced from revoking access.
--
-- Without this migration that switch is a mass revocation. Nobody has ever
-- been granted `model/embed`; they were granted `model/bge-m3` and reached
-- `embed` because authorisation looked at the resolved target. Migration 0029
-- already showed what a silent mass revocation looks like in production, and
-- once is enough.
--
-- Grants on provider models are left alone. They are still what authorises a
-- caller naming a provider model directly, which stays possible until it is
-- removed on purpose rather than as a side effect of this.

-- Every provider model a frontend model can reach: its defaults and every
-- target of every rule it has.
CREATE TEMP TABLE frontend_targets ON COMMIT DROP AS
SELECT fm.id AS frontend_id, fm.name AS frontend_name, d.provider_model_id
  FROM frontend_models fm
  JOIN frontend_model_defaults d ON d.frontend_model_id = fm.id
UNION
SELECT fm.id, fm.name, rt.provider_model_id
  FROM frontend_models fm
  JOIN routing_rules r  ON r.frontend_model_id = fm.id
  JOIN rule_targets  rt ON rt.rule_id = r.id;

-- A role gets the frontend model only if it already holds *every* target.
-- "Every", not "any": a frontend model whose rules can route to two provider
-- models can serve a request from either, so granting it to a role holding
-- only one would widen that role's reach to a model it was never given.
-- Widening access is not something a migration gets to do quietly.
CREATE TEMP TABLE frontend_grants ON COMMIT DROP AS
SELECT r.id AS role_id, t.frontend_name
  FROM roles r
  JOIN frontend_targets t ON TRUE
  JOIN provider_models pm ON pm.id = t.provider_model_id
 GROUP BY r.id, t.frontend_id, t.frontend_name
HAVING bool_and(EXISTS (
           SELECT 1
             FROM role_permissions rp
             JOIN permissions p ON p.id = rp.permission_id
            WHERE rp.role_id = r.id
              AND p.verb = 'model:invoke'
              AND (p.resource = 'model/*' OR p.resource = 'model/' || pm.name)));

INSERT INTO permissions (verb, resource)
SELECT DISTINCT 'model:invoke', 'model/' || frontend_name FROM frontend_grants
ON CONFLICT (verb, resource) DO NOTHING;

INSERT INTO role_permissions (role_id, permission_id)
SELECT g.role_id, p.id
  FROM frontend_grants g
  JOIN permissions p ON p.verb = 'model:invoke'
                    AND p.resource = 'model/' || g.frontend_name
ON CONFLICT DO NOTHING;
