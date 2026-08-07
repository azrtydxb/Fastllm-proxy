-- 0003_bootstrap_principal_and_operator_grants.sql grants `inference` to the
-- bootstrap principal with a plain INSERT, unlike its sibling INSERT INTO
-- principals two statements above (which has ON CONFLICT (id) DO NOTHING for
-- exactly this reason). A database that already holds that
-- (principal_id, role_id) row when 0003 runs — an operator who granted it by
-- hand before migrating, or a retry after a partial failure — hits
-- principal_roles's primary key and fails the migration outright.
--
-- 0003 itself cannot be edited to add the missing ON CONFLICT DO NOTHING:
-- it has already run (and been checksummed) against every database that
-- deployed any control-plane release before this one, and sqlx's migrator
-- refuses to start if an applied migration's file content no longer matches
-- the checksum it recorded. So this ships the fix as its own migration
-- instead — idempotent by construction, safe whether the row from 0003
-- already exists (the common case, on any database that already migrated)
-- or does not (a brand-new database that reaches this file before ever
-- having granted the role by hand).
INSERT INTO principal_roles (principal_id, role_id)
SELECT 1, r.id FROM roles r WHERE r.name = 'inference'
ON CONFLICT DO NOTHING;
