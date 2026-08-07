-- The bootstrap principal must be able to administer, or a fresh install is
-- unusable.
--
-- Migration 0003 seeded principal 1 and gave it `inference` so that keys
-- minted against it could invoke models. That was enough while `/admin/*` had
-- no authorisation. Once admin routes started requiring permissions, the one
-- account an operator can actually log into (`set-password --name bootstrap`,
-- the documented first step) held nothing but `model:invoke` — so it
-- authenticated successfully and then got 403 from every admin route,
-- including the ones needed to grant itself anything better. The only exit was
-- hand-written SQL, which is precisely what the admin API exists to replace.
--
-- Granting `admin` here is safe in the way that matters: admin permissions are
-- only ever consulted for a *session*, never for an API key. A key minted
-- against this principal still reaches exactly the models `inference` grants
-- and cannot touch `/admin/*` at all, because keys do not create sessions.
INSERT INTO principal_roles (principal_id, role_id)
SELECT 1, r.id FROM roles r WHERE r.name = 'admin'
ON CONFLICT DO NOTHING;
