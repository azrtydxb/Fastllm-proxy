-- Restore the grants migration 0029 revoked.
--
-- 0029 split a model that had several backends into one provider model per
-- provider, qualified its name, and put the original name on a frontend model
-- so callers were unaffected. Callers *were* affected, in a way nothing
-- reported: `proxy.rs` authorises against the resolved concrete model, never
-- the frontend name -- "a virtual model routes access; it must never be able
-- to grant it" -- so a principal holding `model:invoke` on `model/bge-m3`
-- reached the frontend model, resolved to `bge-m3@192.168.10.245:8890`, and
-- was refused for a model it had never heard of.
--
-- Found by making the call. The first real request after 0029 reached the dev
-- cluster came back `403 model_access_denied`, and two live roles had lost
-- access silently.
--
-- This is a separate migration rather than a correction to 0029 because 0029
-- has been applied: `sqlx` checksums migrations and refuses to start when an
-- applied one changes, and rolling a database backwards to re-apply an amended
-- one is not something to do to fix a bug that can be fixed forwards.
--
-- The deeper problem -- that a grant is pinned to a string the system is free
-- to rewrite -- is not fixed here. Authorisation moves to the frontend model,
-- which is what makes renaming a provider model safe. Until then, anything
-- that renames one has to do what this migration does.

-- A model 0029 split, recovered from the shape it left behind: a frontend
-- model whose target is named `<frontend name>@<something>`. That pattern is
-- exactly what 0029 wrote and nothing else produces it, since the admin API
-- has no way to rename a model (`PatchModel` carries no `name`).
CREATE TEMP TABLE split_names ON COMMIT DROP AS
SELECT DISTINCT v.name AS original_name, m.name AS split_name
FROM virtual_models v
JOIN virtual_model_defaults d ON d.virtual_model_id = v.id
JOIN models m ON m.id = d.model_id
WHERE m.name LIKE v.name || '@%';

-- The permission row for each new name, where a grant on the old one exists.
INSERT INTO permissions (verb, resource)
SELECT DISTINCT p.verb, 'model/' || s.split_name
FROM split_names s
JOIN permissions p ON p.resource = 'model/' || s.original_name
ON CONFLICT (verb, resource) DO NOTHING;

-- And the same roles that held the old one. The old permission is left in
-- place: it costs nothing, it is what a caller naming the frontend model will
-- be authorised against once authorisation moves there, and deleting it would
-- be a second silent revocation.
INSERT INTO role_permissions (role_id, permission_id)
SELECT DISTINCT rp.role_id, np.id
FROM split_names s
JOIN permissions op ON op.resource = 'model/' || s.original_name
JOIN role_permissions rp ON rp.permission_id = op.id
JOIN permissions np ON np.verb = op.verb
                   AND np.resource = 'model/' || s.split_name
ON CONFLICT DO NOTHING;
