-- Load balancing was a policy you set on a list, in three different places:
-- on a frontend model (governing its defaults), on each rule (governing that
-- rule's targets), and on a provider model (governing its own attachments).
-- The first two overlapped, neither was reusable, and where a given policy
-- applied was not visible from the screen it was set on.
--
-- A pool is the same idea made into an object with a name. `leastloaded-gemma`
-- and `lowestlatency-gemma` can hold the same members and differ only in how
-- they choose, and either can be pointed at from as many rules as like. That
-- is what a policy buried in a list could never do.
--
-- The division that falls out of it, and the reason this is a simplification
-- rather than a fourth place to look:
--
--   a rule's targets  -- an ordered failover chain, tried in order
--   a pool            -- how to choose between several models at once
--   a provider model  -- how to choose between its own providers
--
-- Each answers a different question, and each is set in exactly one place.
CREATE TABLE model_pools (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name        text NOT NULL UNIQUE,
    description text NOT NULL DEFAULT '',
    -- NULL is the weighted split, which is what a target list has always meant
    -- and stays the default here: a deterministic pick on the request prefix,
    -- so a conversation stays on one member rather than flipping per request.
    policy      text CHECK (policy IS NULL OR policy IN
                    ('cache-affinity', 'least-loaded', 'round-robin',
                     'lowest-latency', 'cheapest'))
);

CREATE TABLE model_pool_members (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    pool_id           uuid NOT NULL REFERENCES model_pools(id) ON DELETE CASCADE,
    provider_model_id uuid NOT NULL REFERENCES provider_models(id) ON DELETE CASCADE,
    weight            integer NOT NULL DEFAULT 100 CHECK (weight >= 0),
    position          integer NOT NULL,
    -- A member twice over is one machine counted twice: the weighted split
    -- would give it double share and least-loaded would compare it with
    -- itself.
    UNIQUE (pool_id, provider_model_id)
);

CREATE INDEX model_pool_members_by_pool ON model_pool_members (pool_id);

-- A target is now a provider model *or* a pool. Bound by name with the id as a
-- cache, exactly as `provider_model_id` already is -- so a pool that is deleted
-- and recreated under the same name reattaches, and renaming one carries.
ALTER TABLE rule_targets
    ADD COLUMN model_pool_id uuid REFERENCES model_pools(id) ON DELETE SET NULL;
ALTER TABLE frontend_model_defaults
    ADD COLUMN model_pool_id uuid REFERENCES model_pools(id) ON DELETE SET NULL;

-- And the two policies a pool replaces are gone. Leaving them would mean load
-- balancing could still be configured in three places while only one of them
-- was documented, which is the state this migration exists to end.
--
-- Nothing is lost: a rule that balanced across its targets is a rule pointing
-- at a pool, and the rule's own target list goes back to meaning what it says
-- on the screen -- try these in order.
ALTER TABLE routing_rules DROP COLUMN policy;
ALTER TABLE frontend_models DROP COLUMN policy;
