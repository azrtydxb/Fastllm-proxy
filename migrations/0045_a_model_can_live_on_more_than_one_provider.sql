-- A provider model is an identity; where it runs is a separate row.
--
-- Until now a provider model belonged to exactly one provider, so one model
-- served by two machines had to be entered twice. That is why this deployment
-- holds `bge-m3@192.168.10.245:8890` and `bge-m3@192.168.10.246:8890`: one
-- model, one set of weights, two rows, two prices to keep in step and two
-- context windows to keep equal.
--
-- The cost was not only cosmetic. `Registry` groups backends into a pool per
-- model name and `router.rs` chooses within that pool -- prefix-cache
-- affinity, least-loaded, lowest-latency. With one provider per model every
-- pool held exactly one backend, so the balancer picked one item from a list
-- of one on every request. The policy that matters most on prefix-caching
-- engines was switched off by the schema rather than by configuration.
--
-- What stays on the model is what the weights determine: name, context
-- window, description, cache TTL, whether it is the deployment fallback.
--
-- What moves here is what the *provider* determines, and prices are in that
-- list deliberately: the same model costs different amounts at different
-- vendors, so a price is a fact about an attachment and not about a model.
-- `upstream_model` moves for the same reason -- OpenRouter calls it
-- `google/gemini-2.5-flash` and Google calls it `gemini-2.5-flash` -- and it
-- is the field that forced a separate model per provider even when nothing
-- else differed.
--
-- The credential, protocol and auth headers stay on `providers`. This is the
-- join table that decomposition should have had, not a retreat from it.
CREATE TABLE model_backends (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    provider_model_id     uuid NOT NULL REFERENCES provider_models(id) ON DELETE CASCADE,
    provider_id           uuid NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    -- What this provider calls the model. NULL means "the same as the model's
    -- own name", which is the common case for a self-hosted engine.
    upstream_model        text,
    input_price_per_mtok  bigint CHECK (input_price_per_mtok IS NULL OR input_price_per_mtok >= 0),
    output_price_per_mtok bigint CHECK (output_price_per_mtok IS NULL OR output_price_per_mtok >= 0),
    default_max_tokens    integer CHECK (default_max_tokens IS NULL OR default_max_tokens > 0),
    -- One attachment per provider per model. A second row for the same pair
    -- would be two names for one endpoint, which routing would treat as two
    -- backends and load-balance between -- sending half the traffic to a
    -- machine it already counted.
    UNIQUE (provider_model_id, provider_id)
);

CREATE INDEX model_backends_by_model ON model_backends (provider_model_id);
CREATE INDEX model_backends_by_provider ON model_backends (provider_id);

-- Every model that had a provider gets exactly one attachment, carrying the
-- fields unchanged. A model with no provider -- the detached state a backend
-- deletion used to leave behind -- gets none, and is now simply a model with
-- an empty pool rather than a row with five NULL columns meaning the same
-- thing.
INSERT INTO model_backends (provider_model_id, provider_id, upstream_model,
                            input_price_per_mtok, output_price_per_mtok, default_max_tokens)
SELECT id, provider_id, upstream_model,
       input_price_per_mtok, output_price_per_mtok, default_max_tokens
FROM provider_models
WHERE provider_id IS NOT NULL;

-- Dropped rather than left in place: two sources of truth for a price is how
-- a deployment ends up billing from one and displaying the other.
ALTER TABLE provider_models
    DROP COLUMN provider_id,
    DROP COLUMN upstream_model,
    DROP COLUMN input_price_per_mtok,
    DROP COLUMN output_price_per_mtok,
    DROP COLUMN default_max_tokens;

-- Which attachment served, so a request is priced at the rate of the provider
-- that actually answered it rather than at whichever price the model happens
-- to list first. ON DELETE SET NULL for the same reason the other usage
-- foreign keys are: the traffic happened, and deleting a backend must not
-- delete the record of what it cost.
ALTER TABLE usage_events
    ADD COLUMN model_backend_id uuid REFERENCES model_backends(id) ON DELETE SET NULL;

-- `target_provider_name` existed for one reason, stated in its own code: "the
-- same model name on two hosts is the normal case, and re-attaching to
-- whichever row was found first would silently repoint a frontend model at a
-- different host." That case no longer exists. Two hosts serving one model are
-- now one provider model with two attachments, `provider_models.name` is
-- unique, and a rule target names a model -- which providers serve it is the
-- model's business, not the target's.
--
-- Dropped rather than left unmaintained: a column that is right for
-- single-provider models and stale for the rest is worse than no column, and
-- it would be read by the Frontend models screen as though it were current.
ALTER TABLE rule_targets DROP COLUMN target_provider_name;
ALTER TABLE frontend_model_defaults DROP COLUMN target_provider_name;
