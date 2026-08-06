-- 0001_init.sql seeded roles and permissions but never a principal, and there
-- is no admin route that creates one (POST /admin/keys only ever creates a
-- *key*, against an existing principal_id). A fresh install therefore could
-- not mint a working key at all: the README's first-key example uses
-- principal_id: 1, which does not exist, and the INSERT in
-- control::api::create_key fails its foreign key against `principals` — a
-- failure post_key used to report as a bare 400 with no body explaining why.
--
-- Seed exactly one principal, at a stable, obvious id, so that example works
-- verbatim against a database that has only ever run migrations. `id` is set
-- explicitly (rather than left to BIGSERIAL) so it stays 1 across every
-- environment this migration runs in — a real operator is free to add more
-- principals afterward through direct SQL (there is no admin route for that
-- either yet; see deploy/README.md).
-- ON CONFLICT DO NOTHING: a database that already has a principal at id 1
-- (e.g. inserted by hand via the previously-documented manual route) must
-- not fail this migration — the control plane refusing to start over a
-- redundant seed row is worse than a no-op here. Seeding intent on a fresh
-- database is unchanged.
INSERT INTO principals (id, kind, name) VALUES
    (1, 'service_account', 'bootstrap')
ON CONFLICT (id) DO NOTHING;

-- BIGSERIAL's sequence has no idea a row landed at an explicit id; without
-- this, the next principal created by ordinary means (nextval, i.e. id 1)
-- would collide with the row just inserted above.
SELECT setval(pg_get_serial_sequence('principals', 'id'), (SELECT max(id) FROM principals));

-- Grant `inference` so a key minted against principal_id 1 can actually
-- invoke a model — a principal that exists but is not in any role could
-- still mint a key, but that key would be authenticated and authorised for
-- nothing, which is not a usable "first key".
INSERT INTO principal_roles (principal_id, role_id)
SELECT 1, r.id FROM roles r WHERE r.name = 'inference';

-- 0001_init.sql also seeded `operator` ("Manage models and keys, no user
-- administration") with zero permissions — a role that grants nothing reads
-- as configured but does less than no role at all. This schema has no
-- separate "user administration" permission to withhold, so operator's grant
-- is everything except model:invoke: key and model management, and usage
-- visibility, without the inference role's blanket model-invoke default.
INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id FROM roles r, permissions p
WHERE r.name = 'operator' AND p.verb != 'model:invoke';
