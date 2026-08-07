-- A deployment-wide last resort.
--
-- Routing already fails over *within* a rule's target list, but only to targets
-- that rule named. A request whose whole chain is exhausted — every backend
-- unreachable, every provider rate-limiting, a model whose backends were all
-- dropped for an undecryptable credential — has nowhere left to go and the
-- caller gets the last upstream's error.
--
-- This is the model that catches those. It is appended to every candidate chain
-- as the final entry, for concrete model names as well as virtual ones, so it
-- covers the case a rule author did not think to.
--
-- Partial unique index rather than a settings table: at most one model can be
-- the fallback, and expressing that as a constraint means it cannot drift into
-- two. Marking a different model as the fallback is a two-statement update, and
-- an admin route does it in one transaction.
ALTER TABLE models ADD COLUMN is_fallback BOOLEAN NOT NULL DEFAULT false;

CREATE UNIQUE INDEX models_single_fallback ON models (is_fallback) WHERE is_fallback;
