-- One word for one thing.
--
-- "Backend model" invited exactly the mistake migration 0029 corrected: a
-- model with a list of backends underneath it. It becomes "provider model",
-- which carries the constraint in its name -- a provider model belongs to
-- exactly one provider.
--
-- `virtual_models` goes with it. The screen has always said "Frontend models"
-- while the table said `virtual_models`, and leaving that alone would trade
-- one disagreement for another. `virtual` also says the wrong thing now: a
-- frontend model is not a pretend model, it is the only name a client is meant
-- to use.
--
-- This rides the breaking change 0029 already made rather than adding a second
-- one later for a cosmetic reason. `ALTER TABLE ... RENAME` preserves every
-- row, index and grant; the data does not move.

ALTER TABLE models                 RENAME TO provider_models;
ALTER TABLE virtual_models         RENAME TO frontend_models;
ALTER TABLE virtual_model_defaults RENAME TO frontend_model_defaults;

-- The foreign keys that name what they point at.
ALTER TABLE frontend_model_defaults RENAME COLUMN model_id         TO provider_model_id;
ALTER TABLE frontend_model_defaults RENAME COLUMN virtual_model_id TO frontend_model_id;
ALTER TABLE rule_targets            RENAME COLUMN model_id         TO provider_model_id;
ALTER TABLE routing_rules           RENAME COLUMN virtual_model_id TO frontend_model_id;
ALTER TABLE usage_events            RENAME COLUMN model_id         TO provider_model_id;
ALTER TABLE usage_rollup_hourly     RENAME COLUMN model_id         TO provider_model_id;

-- Index and constraint names are renamed too. Nothing in the code references
-- them -- every `ON CONFLICT` names its columns rather than a constraint --
-- but a schema where `models_pkey` sits on `provider_models` is a schema that
-- reads as half-finished to whoever meets it next.
ALTER INDEX models_pkey                        RENAME TO provider_models_pkey;
ALTER INDEX models_name_key                    RENAME TO provider_models_name_key;
ALTER INDEX models_single_fallback             RENAME TO provider_models_single_fallback;
ALTER INDEX models_provider                    RENAME TO provider_models_provider;
ALTER INDEX models_provider_name               RENAME TO provider_models_provider_name;
ALTER INDEX virtual_models_pkey                RENAME TO frontend_models_pkey;
ALTER INDEX virtual_models_name_key            RENAME TO frontend_models_name_key;
ALTER INDEX virtual_model_defaults_pkey        RENAME TO frontend_model_defaults_pkey;
ALTER INDEX virtual_model_defaults_vm          RENAME TO frontend_model_defaults_fm;
ALTER INDEX virtual_model_defaults_virtual_model_id_position_key
                                               RENAME TO frontend_model_defaults_position_key;
ALTER INDEX usage_events_model                 RENAME TO usage_events_provider_model;

COMMENT ON TABLE provider_models IS
    'A model as one provider exposes it. Exactly one provider (0029), so this '
    'is what a request is routed *to*.';
COMMENT ON TABLE frontend_models IS
    'A name a client asks *for*, resolving by rules and weights to provider '
    'models. The only name a client is meant to use.';
