-- Load balancing distributes work; it does not shop.
--
-- `cheapest` sat in the same list as cache-affinity, least-loaded,
-- lowest-latency and round-robin, and it is not the same kind of thing. The
-- other four read how busy or how warm a candidate is right now and spread
-- traffic accordingly. `cheapest` reads a price, which is a fixed property of
-- a model at a provider and says nothing about load — so a "load balancer" set
-- to it never balanced anything, it just always picked the same member until
-- somebody edited a price.
--
-- Cost still decides routing, through the condition built for it:
-- `min/max_request_cost_micros` on a rule, priced at the cheapest model the
-- frontend model can reach. That is a routing decision, made where routing
-- decisions are made.
--
-- What this gives up is the automatic "prefer whichever vendor is cheaper for
-- this model today". That is now written down instead: order the chain with
-- the cheaper target first, which is a decision an operator can see rather
-- than one the balancer made quietly.
UPDATE provider_models SET policy = NULL WHERE policy = 'cheapest';
UPDATE model_pools     SET policy = NULL WHERE policy = 'cheapest';

ALTER TABLE provider_models DROP CONSTRAINT provider_models_policy_check;
ALTER TABLE provider_models ADD CONSTRAINT provider_models_policy_check
    CHECK (policy IS NULL OR policy IN
        ('cache-affinity', 'least-loaded', 'round-robin', 'lowest-latency'));

ALTER TABLE model_pools DROP CONSTRAINT model_pools_policy_check;
ALTER TABLE model_pools ADD CONSTRAINT model_pools_policy_check
    CHECK (policy IS NULL OR policy IN
        ('cache-affinity', 'least-loaded', 'round-robin', 'lowest-latency'));
