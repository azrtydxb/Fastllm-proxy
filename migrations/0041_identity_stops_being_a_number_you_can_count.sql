-- Identity stops being a number you can count.
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

-- 1. A uuid beside every id.
ALTER TABLE a2a_agents ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE api_keys ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE frontend_model_defaults ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE frontend_models ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE mcp_servers ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE permissions ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE principals ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE prompt_class_examples ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE prompt_classes ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE provider_models ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE providers ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE roles ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE routing_rules ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE rule_targets ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE sessions ADD COLUMN uuid_id UUID NOT NULL DEFAULT gen_random_uuid();

-- 2. A uuid beside every foreign key, resolved through its parent's new column.
ALTER TABLE frontend_model_defaults ADD COLUMN frontend_model_id_uuid UUID;
UPDATE frontend_model_defaults c SET frontend_model_id_uuid = p.uuid_id FROM frontend_models p WHERE p.id = c.frontend_model_id;
ALTER TABLE routing_rules ADD COLUMN frontend_model_id_uuid UUID;
UPDATE routing_rules c SET frontend_model_id_uuid = p.uuid_id FROM frontend_models p WHERE p.id = c.frontend_model_id;
ALTER TABLE role_permissions ADD COLUMN permission_id_uuid UUID;
UPDATE role_permissions c SET permission_id_uuid = p.uuid_id FROM permissions p WHERE p.id = c.permission_id;
ALTER TABLE api_keys ADD COLUMN principal_id_uuid UUID;
UPDATE api_keys c SET principal_id_uuid = p.uuid_id FROM principals p WHERE p.id = c.principal_id;
ALTER TABLE audit_events ADD COLUMN actor_id_uuid UUID;
UPDATE audit_events c SET actor_id_uuid = p.uuid_id FROM principals p WHERE p.id = c.actor_id;
ALTER TABLE budgets ADD COLUMN principal_id_uuid UUID;
UPDATE budgets c SET principal_id_uuid = p.uuid_id FROM principals p WHERE p.id = c.principal_id;
ALTER TABLE limits ADD COLUMN principal_id_uuid UUID;
UPDATE limits c SET principal_id_uuid = p.uuid_id FROM principals p WHERE p.id = c.principal_id;
ALTER TABLE principal_roles ADD COLUMN principal_id_uuid UUID;
UPDATE principal_roles c SET principal_id_uuid = p.uuid_id FROM principals p WHERE p.id = c.principal_id;
ALTER TABLE sessions ADD COLUMN principal_id_uuid UUID;
UPDATE sessions c SET principal_id_uuid = p.uuid_id FROM principals p WHERE p.id = c.principal_id;
ALTER TABLE usage_events ADD COLUMN principal_id_uuid UUID;
UPDATE usage_events c SET principal_id_uuid = p.uuid_id FROM principals p WHERE p.id = c.principal_id;
ALTER TABLE prompt_class_examples ADD COLUMN class_id_uuid UUID;
UPDATE prompt_class_examples c SET class_id_uuid = p.uuid_id FROM prompt_classes p WHERE p.id = c.class_id;
ALTER TABLE prompt_class_refines ADD COLUMN class_id_uuid UUID;
UPDATE prompt_class_refines c SET class_id_uuid = p.uuid_id FROM prompt_classes p WHERE p.id = c.class_id;
ALTER TABLE frontend_model_defaults ADD COLUMN provider_model_id_uuid UUID;
UPDATE frontend_model_defaults c SET provider_model_id_uuid = p.uuid_id FROM provider_models p WHERE p.id = c.provider_model_id;
ALTER TABLE rule_targets ADD COLUMN provider_model_id_uuid UUID;
UPDATE rule_targets c SET provider_model_id_uuid = p.uuid_id FROM provider_models p WHERE p.id = c.provider_model_id;
ALTER TABLE usage_events ADD COLUMN provider_model_id_uuid UUID;
UPDATE usage_events c SET provider_model_id_uuid = p.uuid_id FROM provider_models p WHERE p.id = c.provider_model_id;
ALTER TABLE provider_models ADD COLUMN provider_id_uuid UUID;
UPDATE provider_models c SET provider_id_uuid = p.uuid_id FROM providers p WHERE p.id = c.provider_id;
ALTER TABLE principal_roles ADD COLUMN role_id_uuid UUID;
UPDATE principal_roles c SET role_id_uuid = p.uuid_id FROM roles p WHERE p.id = c.role_id;
ALTER TABLE role_permissions ADD COLUMN role_id_uuid UUID;
UPDATE role_permissions c SET role_id_uuid = p.uuid_id FROM roles p WHERE p.id = c.role_id;
ALTER TABLE rule_targets ADD COLUMN rule_id_uuid UUID;
UPDATE rule_targets c SET rule_id_uuid = p.uuid_id FROM routing_rules p WHERE p.id = c.rule_id;

-- 3. The same for usage_rollup_hourly, which must happen here, while the
--    bigints on both sides still exist to join on.
ALTER TABLE usage_rollup_hourly ADD COLUMN provider_model_id_uuid UUID;
UPDATE usage_rollup_hourly r SET provider_model_id_uuid = p.uuid_id FROM provider_models p WHERE p.id = r.provider_model_id;
ALTER TABLE usage_rollup_hourly ADD COLUMN principal_id_uuid UUID;
UPDATE usage_rollup_hourly r SET principal_id_uuid = p.uuid_id FROM principals p WHERE p.id = r.principal_id;
-- `principal_id` is NOT NULL and part of the primary key, and this table
-- already holds a row whose principal has since been deleted -- there was
-- never a constraint to prevent that. The nil uuid is that row's answer: it
-- reads as "a principal that no longer exists", and keeps both the row and the
-- key. The rollup is what survives after `usage_events` are pruned, so a row
-- lost here is history that cannot be recomputed.
UPDATE usage_rollup_hourly SET principal_id_uuid = '00000000-0000-0000-0000-000000000000'
 WHERE principal_id_uuid IS NULL;

-- 4. Nothing may be lost in a join. A row whose parent resolved to NULL while
--    its bigint was not NULL is a link silently dropped, and the whole point
--    of doing this in stages is to find that out before the swap.
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM frontend_model_defaults WHERE frontend_model_id IS NOT NULL AND frontend_model_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'frontend_model_defaults.frontend_model_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM routing_rules WHERE frontend_model_id IS NOT NULL AND frontend_model_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'routing_rules.frontend_model_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM role_permissions WHERE permission_id IS NOT NULL AND permission_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'role_permissions.permission_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM api_keys WHERE principal_id IS NOT NULL AND principal_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'api_keys.principal_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM audit_events WHERE actor_id IS NOT NULL AND actor_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'audit_events.actor_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM budgets WHERE principal_id IS NOT NULL AND principal_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'budgets.principal_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM limits WHERE principal_id IS NOT NULL AND principal_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'limits.principal_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM principal_roles WHERE principal_id IS NOT NULL AND principal_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'principal_roles.principal_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM sessions WHERE principal_id IS NOT NULL AND principal_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'sessions.principal_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM usage_events WHERE principal_id IS NOT NULL AND principal_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'usage_events.principal_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM prompt_class_examples WHERE class_id IS NOT NULL AND class_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'prompt_class_examples.class_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM prompt_class_refines WHERE class_id IS NOT NULL AND class_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'prompt_class_refines.class_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM frontend_model_defaults WHERE provider_model_id IS NOT NULL AND provider_model_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'frontend_model_defaults.provider_model_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM rule_targets WHERE provider_model_id IS NOT NULL AND provider_model_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'rule_targets.provider_model_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM usage_events WHERE provider_model_id IS NOT NULL AND provider_model_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'usage_events.provider_model_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM provider_models WHERE provider_id IS NOT NULL AND provider_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'provider_models.provider_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM principal_roles WHERE role_id IS NOT NULL AND role_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'principal_roles.role_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM role_permissions WHERE role_id IS NOT NULL AND role_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'role_permissions.role_id did not resolve for every row';
  END IF;
END $$;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM rule_targets WHERE rule_id IS NOT NULL AND rule_id_uuid IS NULL) THEN
    RAISE EXCEPTION 'rule_targets.rule_id did not resolve for every row';
  END IF;
END $$;

-- 5. Drop the foreign keys by lookup, never by name. Four of these kept the
--    name they were given before their tables were renamed
--    (models_provider_id_fkey, usage_events_model_id_fkey,
--    routing_rules_virtual_model_id_fkey,
--    virtual_model_defaults_virtual_model_id_fkey), because a constraint name
--    survives a rename. Assuming the convention would have left them in place
--    and failed the column drop below.
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'frontend_model_defaults' AND k.column_name = 'frontend_model_id'
  LOOP EXECUTE format('ALTER TABLE frontend_model_defaults DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'routing_rules' AND k.column_name = 'frontend_model_id'
  LOOP EXECUTE format('ALTER TABLE routing_rules DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'role_permissions' AND k.column_name = 'permission_id'
  LOOP EXECUTE format('ALTER TABLE role_permissions DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'api_keys' AND k.column_name = 'principal_id'
  LOOP EXECUTE format('ALTER TABLE api_keys DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'audit_events' AND k.column_name = 'actor_id'
  LOOP EXECUTE format('ALTER TABLE audit_events DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'budgets' AND k.column_name = 'principal_id'
  LOOP EXECUTE format('ALTER TABLE budgets DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'limits' AND k.column_name = 'principal_id'
  LOOP EXECUTE format('ALTER TABLE limits DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'principal_roles' AND k.column_name = 'principal_id'
  LOOP EXECUTE format('ALTER TABLE principal_roles DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'sessions' AND k.column_name = 'principal_id'
  LOOP EXECUTE format('ALTER TABLE sessions DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'usage_events' AND k.column_name = 'principal_id'
  LOOP EXECUTE format('ALTER TABLE usage_events DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'prompt_class_examples' AND k.column_name = 'class_id'
  LOOP EXECUTE format('ALTER TABLE prompt_class_examples DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'prompt_class_refines' AND k.column_name = 'class_id'
  LOOP EXECUTE format('ALTER TABLE prompt_class_refines DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'frontend_model_defaults' AND k.column_name = 'provider_model_id'
  LOOP EXECUTE format('ALTER TABLE frontend_model_defaults DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'rule_targets' AND k.column_name = 'provider_model_id'
  LOOP EXECUTE format('ALTER TABLE rule_targets DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'usage_events' AND k.column_name = 'provider_model_id'
  LOOP EXECUTE format('ALTER TABLE usage_events DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'provider_models' AND k.column_name = 'provider_id'
  LOOP EXECUTE format('ALTER TABLE provider_models DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'principal_roles' AND k.column_name = 'role_id'
  LOOP EXECUTE format('ALTER TABLE principal_roles DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'role_permissions' AND k.column_name = 'role_id'
  LOOP EXECUTE format('ALTER TABLE role_permissions DROP CONSTRAINT %I', c); END LOOP;
END $$;
DO $$ DECLARE c TEXT; BEGIN
  FOR c IN SELECT tc.constraint_name FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_name = tc.constraint_name
            WHERE tc.constraint_type = 'FOREIGN KEY'
              AND tc.table_name = 'rule_targets' AND k.column_name = 'rule_id'
  LOOP EXECUTE format('ALTER TABLE rule_targets DROP CONSTRAINT %I', c); END LOOP;
END $$;

-- 6. Swap the foreign key columns.
ALTER TABLE frontend_model_defaults DROP COLUMN frontend_model_id;
ALTER TABLE frontend_model_defaults RENAME COLUMN frontend_model_id_uuid TO frontend_model_id;
ALTER TABLE frontend_model_defaults ALTER COLUMN frontend_model_id SET NOT NULL;
ALTER TABLE routing_rules DROP COLUMN frontend_model_id;
ALTER TABLE routing_rules RENAME COLUMN frontend_model_id_uuid TO frontend_model_id;
ALTER TABLE routing_rules ALTER COLUMN frontend_model_id SET NOT NULL;
ALTER TABLE role_permissions DROP COLUMN permission_id;
ALTER TABLE role_permissions RENAME COLUMN permission_id_uuid TO permission_id;
ALTER TABLE role_permissions ALTER COLUMN permission_id SET NOT NULL;
ALTER TABLE api_keys DROP COLUMN principal_id;
ALTER TABLE api_keys RENAME COLUMN principal_id_uuid TO principal_id;
ALTER TABLE api_keys ALTER COLUMN principal_id SET NOT NULL;
ALTER TABLE audit_events DROP COLUMN actor_id;
ALTER TABLE audit_events RENAME COLUMN actor_id_uuid TO actor_id;
ALTER TABLE budgets DROP COLUMN principal_id;
ALTER TABLE budgets RENAME COLUMN principal_id_uuid TO principal_id;
ALTER TABLE budgets ALTER COLUMN principal_id SET NOT NULL;
ALTER TABLE limits DROP COLUMN principal_id;
ALTER TABLE limits RENAME COLUMN principal_id_uuid TO principal_id;
ALTER TABLE limits ALTER COLUMN principal_id SET NOT NULL;
ALTER TABLE principal_roles DROP COLUMN principal_id;
ALTER TABLE principal_roles RENAME COLUMN principal_id_uuid TO principal_id;
ALTER TABLE principal_roles ALTER COLUMN principal_id SET NOT NULL;
ALTER TABLE sessions DROP COLUMN principal_id;
ALTER TABLE sessions RENAME COLUMN principal_id_uuid TO principal_id;
ALTER TABLE sessions ALTER COLUMN principal_id SET NOT NULL;
ALTER TABLE usage_events DROP COLUMN principal_id;
ALTER TABLE usage_events RENAME COLUMN principal_id_uuid TO principal_id;
ALTER TABLE usage_events ALTER COLUMN principal_id SET NOT NULL;
ALTER TABLE prompt_class_examples DROP COLUMN class_id;
ALTER TABLE prompt_class_examples RENAME COLUMN class_id_uuid TO class_id;
ALTER TABLE prompt_class_examples ALTER COLUMN class_id SET NOT NULL;
ALTER TABLE prompt_class_refines DROP COLUMN class_id;
ALTER TABLE prompt_class_refines RENAME COLUMN class_id_uuid TO class_id;
ALTER TABLE prompt_class_refines ALTER COLUMN class_id SET NOT NULL;
ALTER TABLE frontend_model_defaults DROP COLUMN provider_model_id;
ALTER TABLE frontend_model_defaults RENAME COLUMN provider_model_id_uuid TO provider_model_id;
ALTER TABLE rule_targets DROP COLUMN provider_model_id;
ALTER TABLE rule_targets RENAME COLUMN provider_model_id_uuid TO provider_model_id;
ALTER TABLE usage_events DROP COLUMN provider_model_id;
ALTER TABLE usage_events RENAME COLUMN provider_model_id_uuid TO provider_model_id;
ALTER TABLE provider_models DROP COLUMN provider_id;
ALTER TABLE provider_models RENAME COLUMN provider_id_uuid TO provider_id;
ALTER TABLE provider_models ALTER COLUMN provider_id SET NOT NULL;
ALTER TABLE principal_roles DROP COLUMN role_id;
ALTER TABLE principal_roles RENAME COLUMN role_id_uuid TO role_id;
ALTER TABLE principal_roles ALTER COLUMN role_id SET NOT NULL;
ALTER TABLE role_permissions DROP COLUMN role_id;
ALTER TABLE role_permissions RENAME COLUMN role_id_uuid TO role_id;
ALTER TABLE role_permissions ALTER COLUMN role_id SET NOT NULL;
ALTER TABLE rule_targets DROP COLUMN rule_id;
ALTER TABLE rule_targets RENAME COLUMN rule_id_uuid TO rule_id;
ALTER TABLE rule_targets ALTER COLUMN rule_id SET NOT NULL;

-- 7. Swap the rollup's, and rebuild the key they are part of.
ALTER TABLE usage_rollup_hourly DROP CONSTRAINT usage_rollup_hourly_pkey;
ALTER TABLE usage_rollup_hourly DROP COLUMN provider_model_id;
ALTER TABLE usage_rollup_hourly RENAME COLUMN provider_model_id_uuid TO provider_model_id;
ALTER TABLE usage_rollup_hourly DROP COLUMN principal_id;
ALTER TABLE usage_rollup_hourly RENAME COLUMN principal_id_uuid TO principal_id;
ALTER TABLE usage_rollup_hourly ALTER COLUMN principal_id SET NOT NULL;
ALTER TABLE usage_rollup_hourly ADD PRIMARY KEY (hour, model_name, principal_id);

-- 8. Swap the primary keys themselves.
ALTER TABLE a2a_agents DROP CONSTRAINT a2a_agents_pkey CASCADE;
ALTER TABLE a2a_agents DROP COLUMN id;
ALTER TABLE a2a_agents RENAME COLUMN uuid_id TO id;
ALTER TABLE a2a_agents ADD PRIMARY KEY (id);
ALTER TABLE api_keys DROP CONSTRAINT api_keys_pkey CASCADE;
ALTER TABLE api_keys DROP COLUMN id;
ALTER TABLE api_keys RENAME COLUMN uuid_id TO id;
ALTER TABLE api_keys ADD PRIMARY KEY (id);
ALTER TABLE frontend_model_defaults DROP CONSTRAINT frontend_model_defaults_pkey CASCADE;
ALTER TABLE frontend_model_defaults DROP COLUMN id;
ALTER TABLE frontend_model_defaults RENAME COLUMN uuid_id TO id;
ALTER TABLE frontend_model_defaults ADD PRIMARY KEY (id);
ALTER TABLE frontend_models DROP CONSTRAINT frontend_models_pkey CASCADE;
ALTER TABLE frontend_models DROP COLUMN id;
ALTER TABLE frontend_models RENAME COLUMN uuid_id TO id;
ALTER TABLE frontend_models ADD PRIMARY KEY (id);
ALTER TABLE mcp_servers DROP CONSTRAINT mcp_servers_pkey CASCADE;
ALTER TABLE mcp_servers DROP COLUMN id;
ALTER TABLE mcp_servers RENAME COLUMN uuid_id TO id;
ALTER TABLE mcp_servers ADD PRIMARY KEY (id);
ALTER TABLE permissions DROP CONSTRAINT permissions_pkey CASCADE;
ALTER TABLE permissions DROP COLUMN id;
ALTER TABLE permissions RENAME COLUMN uuid_id TO id;
ALTER TABLE permissions ADD PRIMARY KEY (id);
ALTER TABLE principals DROP CONSTRAINT principals_pkey CASCADE;
ALTER TABLE principals DROP COLUMN id;
ALTER TABLE principals RENAME COLUMN uuid_id TO id;
ALTER TABLE principals ADD PRIMARY KEY (id);
ALTER TABLE prompt_class_examples DROP CONSTRAINT prompt_class_examples_pkey CASCADE;
ALTER TABLE prompt_class_examples DROP COLUMN id;
ALTER TABLE prompt_class_examples RENAME COLUMN uuid_id TO id;
ALTER TABLE prompt_class_examples ADD PRIMARY KEY (id);
ALTER TABLE prompt_classes DROP CONSTRAINT prompt_classes_pkey CASCADE;
ALTER TABLE prompt_classes DROP COLUMN id;
ALTER TABLE prompt_classes RENAME COLUMN uuid_id TO id;
ALTER TABLE prompt_classes ADD PRIMARY KEY (id);
ALTER TABLE provider_models DROP CONSTRAINT provider_models_pkey CASCADE;
ALTER TABLE provider_models DROP COLUMN id;
ALTER TABLE provider_models RENAME COLUMN uuid_id TO id;
ALTER TABLE provider_models ADD PRIMARY KEY (id);
ALTER TABLE providers DROP CONSTRAINT providers_pkey CASCADE;
ALTER TABLE providers DROP COLUMN id;
ALTER TABLE providers RENAME COLUMN uuid_id TO id;
ALTER TABLE providers ADD PRIMARY KEY (id);
ALTER TABLE roles DROP CONSTRAINT roles_pkey CASCADE;
ALTER TABLE roles DROP COLUMN id;
ALTER TABLE roles RENAME COLUMN uuid_id TO id;
ALTER TABLE roles ADD PRIMARY KEY (id);
ALTER TABLE routing_rules DROP CONSTRAINT routing_rules_pkey CASCADE;
ALTER TABLE routing_rules DROP COLUMN id;
ALTER TABLE routing_rules RENAME COLUMN uuid_id TO id;
ALTER TABLE routing_rules ADD PRIMARY KEY (id);
ALTER TABLE rule_targets DROP CONSTRAINT rule_targets_pkey CASCADE;
ALTER TABLE rule_targets DROP COLUMN id;
ALTER TABLE rule_targets RENAME COLUMN uuid_id TO id;
ALTER TABLE rule_targets ADD PRIMARY KEY (id);
ALTER TABLE sessions DROP CONSTRAINT sessions_pkey CASCADE;
ALTER TABLE sessions DROP COLUMN id;
ALTER TABLE sessions RENAME COLUMN uuid_id TO id;
ALTER TABLE sessions ADD PRIMARY KEY (id);

-- 9. Re-establish every edge with the delete rule it had.
ALTER TABLE frontend_model_defaults ADD CONSTRAINT frontend_model_defaults_frontend_model_id_fkey FOREIGN KEY (frontend_model_id) REFERENCES frontend_models(id) ON DELETE CASCADE;
ALTER TABLE routing_rules ADD CONSTRAINT routing_rules_frontend_model_id_fkey FOREIGN KEY (frontend_model_id) REFERENCES frontend_models(id) ON DELETE CASCADE;
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_permission_id_fkey FOREIGN KEY (permission_id) REFERENCES permissions(id) ON DELETE CASCADE;
ALTER TABLE api_keys ADD CONSTRAINT api_keys_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE;
ALTER TABLE audit_events ADD CONSTRAINT audit_events_actor_id_fkey FOREIGN KEY (actor_id) REFERENCES principals(id) ON DELETE SET NULL;
ALTER TABLE budgets ADD CONSTRAINT budgets_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE;
ALTER TABLE limits ADD CONSTRAINT limits_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE;
ALTER TABLE principal_roles ADD CONSTRAINT principal_roles_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE;
ALTER TABLE sessions ADD CONSTRAINT sessions_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE;
ALTER TABLE usage_events ADD CONSTRAINT usage_events_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES principals(id) ON DELETE CASCADE;
ALTER TABLE prompt_class_examples ADD CONSTRAINT prompt_class_examples_class_id_fkey FOREIGN KEY (class_id) REFERENCES prompt_classes(id) ON DELETE CASCADE;
ALTER TABLE prompt_class_refines ADD CONSTRAINT prompt_class_refines_class_id_fkey FOREIGN KEY (class_id) REFERENCES prompt_classes(id) ON DELETE CASCADE;
ALTER TABLE frontend_model_defaults ADD CONSTRAINT frontend_model_defaults_provider_model_id_fkey FOREIGN KEY (provider_model_id) REFERENCES provider_models(id) ON DELETE SET NULL;
ALTER TABLE rule_targets ADD CONSTRAINT rule_targets_provider_model_id_fkey FOREIGN KEY (provider_model_id) REFERENCES provider_models(id) ON DELETE SET NULL;
ALTER TABLE usage_events ADD CONSTRAINT usage_events_provider_model_id_fkey FOREIGN KEY (provider_model_id) REFERENCES provider_models(id) ON DELETE SET NULL;
ALTER TABLE provider_models ADD CONSTRAINT provider_models_provider_id_fkey FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE;
ALTER TABLE principal_roles ADD CONSTRAINT principal_roles_role_id_fkey FOREIGN KEY (role_id) REFERENCES roles(id) ON DELETE CASCADE;
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_role_id_fkey FOREIGN KEY (role_id) REFERENCES roles(id) ON DELETE CASCADE;
ALTER TABLE rule_targets ADD CONSTRAINT rule_targets_rule_id_fkey FOREIGN KEY (rule_id) REFERENCES routing_rules(id) ON DELETE CASCADE;

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
CREATE INDEX IF NOT EXISTS api_keys_principal     ON api_keys(principal_id);
