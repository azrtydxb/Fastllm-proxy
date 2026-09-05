#!/usr/bin/env python3
"""Emit the UUID migration from the live FK graph, so no edge is hand-typed."""

# Everything whose id is an *identity* -- a thing an operator names, an API
# addresses, and another row points at.
#
# `audit_events` and `usage_events` are deliberately absent. Their ids are not
# identities but cursors into an append-only log: the audit listing pages with
# `WHERE id < $1 ORDER BY id DESC`, which means "newer than" only because the
# id is a sequence. A random uuid would leave that query running and quietly
# stop it returning the newest events first. Their *foreign keys* still become
# uuids, because those name things that do have identity.
TABLES = ["a2a_agents","api_keys","frontend_model_defaults","frontend_models",
          "mcp_servers","permissions","principals","prompt_class_examples","prompt_classes",
          "provider_models","providers","roles","routing_rules","rule_targets","sessions"]

FKS = [
 ("frontend_model_defaults","frontend_model_id","frontend_models","CASCADE"),
 ("routing_rules","frontend_model_id","frontend_models","CASCADE"),
 ("role_permissions","permission_id","permissions","CASCADE"),
 ("api_keys","principal_id","principals","CASCADE"),
 ("audit_events","actor_id","principals","SET NULL"),
 ("budgets","principal_id","principals","CASCADE"),
 ("limits","principal_id","principals","CASCADE"),
 ("principal_roles","principal_id","principals","CASCADE"),
 ("sessions","principal_id","principals","CASCADE"),
 ("usage_events","principal_id","principals","CASCADE"),
 ("prompt_class_examples","class_id","prompt_classes","CASCADE"),
 ("prompt_class_refines","class_id","prompt_classes","CASCADE"),
 ("frontend_model_defaults","provider_model_id","provider_models","SET NULL"),
 ("rule_targets","provider_model_id","provider_models","SET NULL"),
 ("usage_events","provider_model_id","provider_models","SET NULL"),
 ("provider_models","provider_id","providers","CASCADE"),
 ("principal_roles","role_id","roles","CASCADE"),
 ("role_permissions","role_id","roles","CASCADE"),
 ("rule_targets","rule_id","routing_rules","CASCADE"),
]
NULLABLE = {(c, col) for c, col, _p, rule in FKS if rule == "SET NULL"}

# usage_rollup_hourly references these two by id and has no foreign key on
# either, so nothing would have complained when they stopped resolving.
ROLLUP = [("provider_model_id", "provider_models", "null"),
          ("principal_id",      "principals",      "nil-uuid")]

out = []
w = out.append

w("""-- Identity stops being a number you can count.
--
-- Every table already had a BIGSERIAL id with real foreign keys, so links
-- already survived a rename; that half was fixed by making targets resolve by
-- id and by carrying grants through a rename. What a sequential id still
-- leaked was everything else. `/admin/provider-models/6351` says how many
-- models this deployment has ever had and what the next one will be called,
-- and now that an id is the stable thing an operator and the API both hold on
-- to, it should not also be a count.
--
-- `gen_random_uuid()` is v4: entirely random. A v7 time-ordered uuid indexes
-- better and tells anyone holding one when the row was created, which is a
-- property being deliberately removed here.
--
-- Add, backfill, verify, swap -- rather than an in-place ALTER TYPE. The new
-- column is populated and checked while the old one is still the key, so a
-- failure anywhere before the swap leaves a working database. `usage_events`
-- is the only table where this is more than a rounding error (285k rows when
-- this was written) and it is rewritten once here rather than carrying a
-- mapping table for ever.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- 1. A uuid beside every id.""")
for t in TABLES:
    w(f"ALTER TABLE {t} ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();")

w("\n-- 2. A uuid beside every foreign key, resolved through its parent's new column.")
for child, col, parent, _rule in FKS:
    w(f"ALTER TABLE {child} ADD COLUMN {col}_uuid UUID;")
    w(f"UPDATE {child} c SET {col}_uuid = p.uuid_id FROM {parent} p WHERE p.id = c.{col};")

w("""
-- 3. The same for usage_rollup_hourly, which must happen here, while the
--    bigints on both sides still exist to join on.""")
for col, parent, _policy in ROLLUP:
    w(f"ALTER TABLE usage_rollup_hourly ADD COLUMN {col}_uuid UUID;")
    w(f"UPDATE usage_rollup_hourly r SET {col}_uuid = p.uuid_id "
      f"FROM {parent} p WHERE p.id = r.{col};")
w("""-- `principal_id` is NOT NULL and part of the primary key, and this table
-- already holds a row whose principal has since been deleted -- there was
-- never a constraint to prevent that. The nil uuid is that row's answer: it
-- reads as "a principal that no longer exists", and keeps both the row and the
-- key. The rollup is what survives after `usage_events` are pruned, so a row
-- lost here is history that cannot be recomputed.
UPDATE usage_rollup_hourly SET principal_id_uuid = '00000000-0000-0000-0000-000000000000'
 WHERE principal_id_uuid IS NULL;""")

w("""
-- 4. Nothing may be lost in a join. A row whose parent resolved to NULL while
--    its bigint was not NULL is a link silently dropped, and the whole point
--    of doing this in stages is to find that out before the swap.""")
for child, col, _parent, _rule in FKS:
    w(f"""DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM {child} WHERE {col} IS NOT NULL AND {col}_uuid IS NULL) THEN
    RAISE EXCEPTION '{child}.{col} did not resolve for every row';
  END IF;
END $$;""")

w("""
-- 5. Drop the foreign keys by lookup, never by name. Four of these kept the
--    name they were given before their tables were renamed
--    (models_provider_id_fkey, usage_events_model_id_fkey,
--    routing_rules_virtual_model_id_fkey,
--    virtual_model_defaults_virtual_model_id_fkey), because a constraint name
--    survives a rename. Assuming the convention would have left them in place
--    and failed the column drop below.""")
for child, col, _parent, _rule in FKS:
    w(f"""DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = '{child}' AND k.column_name = '{col}'
  LOOP EXECUTE format('ALTER TABLE {child} DROP CONSTRAINT %I', c); END LOOP;
END $$;""")

w("\n-- 6. Swap the foreign key columns.")
for child, col, _parent, _rule in FKS:
    w(f"ALTER TABLE {child} DROP COLUMN {col};")
    w(f"ALTER TABLE {child} RENAME COLUMN {col}_uuid TO {col};")
    if (child, col) not in NULLABLE:
        w(f"ALTER TABLE {child} ALTER COLUMN {col} SET NOT NULL;")

w("""
-- 7. Swap the rollup's, and rebuild the key they are part of.
ALTER TABLE usage_rollup_hourly DROP CONSTRAINT usage_rollup_hourly_pkey;""")
for col, _parent, _policy in ROLLUP:
    w(f"ALTER TABLE usage_rollup_hourly DROP COLUMN {col};")
    w(f"ALTER TABLE usage_rollup_hourly RENAME COLUMN {col}_uuid TO {col};")
w("ALTER TABLE usage_rollup_hourly ALTER COLUMN principal_id SET NOT NULL;")
w("ALTER TABLE usage_rollup_hourly ADD PRIMARY KEY (hour, model_name, principal_id);")

w("\n-- 8. Swap the primary keys themselves.")
for t in TABLES:
    w(f"ALTER TABLE {t} DROP CONSTRAINT {t}_pkey CASCADE;")
    w(f"ALTER TABLE {t} DROP COLUMN id;")
    w(f"ALTER TABLE {t} RENAME COLUMN uuid_id TO id;")
    w(f"ALTER TABLE {t} ADD PRIMARY KEY (id);")

w("\n-- 9. Re-establish every edge with the delete rule it had.")
for child, col, parent, rule in FKS:
    w(f"ALTER TABLE {child} ADD CONSTRAINT {child}_{col}_fkey "
      f"FOREIGN KEY ({col}) REFERENCES {parent}(id) ON DELETE {rule};")

w("""
-- 10. The composite keys that step 8's CASCADE took with their parent's.
ALTER TABLE role_permissions    ADD PRIMARY KEY (role_id, permission_id);
ALTER TABLE principal_roles     ADD PRIMARY KEY (principal_id, role_id);
-- `prompt_class_refines` is (class_id, refines) where `refines` is the parent
-- class's *name*, not an id -- which is why prompt classes are still the one
-- thing here that cannot be renamed.
ALTER TABLE prompt_class_refines ADD PRIMARY KEY (class_id, refines);

-- 11. The lookup indexes CASCADE took with them.
CREATE INDEX IF NOT EXISTS usage_events_principal ON usage_events(principal_id);
CREATE INDEX IF NOT EXISTS usage_events_model     ON usage_events(provider_model_id);
CREATE INDEX IF NOT EXISTS sessions_principal     ON sessions(principal_id);
CREATE INDEX IF NOT EXISTS api_keys_principal     ON api_keys(principal_id);""")

print("\n".join(out))
