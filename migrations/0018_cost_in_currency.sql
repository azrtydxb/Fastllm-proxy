-- What a request costs, in money rather than tokens.
--
-- Budgets were denominated in tokens, which cannot express what an operator
-- actually wants to cap: a token budget treats a request to a frontier model
-- and one to a local 7B as equal when their prices differ by two orders of
-- magnitude. "This team gets $500 a month" was unsayable.
--
-- Micro-dollars as BIGINT, not NUMERIC or a float. Prices per million tokens
-- run to four decimal places and every arithmetic path here is add-and-compare,
-- so an integer in the smallest unit anyone quotes is exact, cheap to sum, and
-- has no rounding mode to get wrong. 1_000_000 = one dollar.
ALTER TABLE models
    -- Per *million* tokens, which is the unit every provider publishes. NULL
    -- means unpriced: usage is still recorded, cost is left NULL rather than
    -- assumed to be zero, so an unpriced model is visibly unpriced instead of
    -- quietly free.
    ADD COLUMN input_price_per_mtok  BIGINT NULL CHECK (input_price_per_mtok  IS NULL OR input_price_per_mtok  >= 0),
    ADD COLUMN output_price_per_mtok BIGINT NULL CHECK (output_price_per_mtok IS NULL OR output_price_per_mtok >= 0);

-- Cost of the request this row records, computed at ingest from the model's
-- price and the tokens reported. Stored rather than derived on read so a later
-- price change does not silently rewrite history: what a request cost is a fact
-- about when it happened.
ALTER TABLE usage_events
    ADD COLUMN cost_micros BIGINT NULL;

-- A budget may cap tokens, money, or both. Both nullable because a deployment
-- with no prices set can still cap tokens, and one that only cares about spend
-- should not have to invent a token number.
ALTER TABLE budgets
    ALTER COLUMN tokens_total DROP NOT NULL,
    ADD COLUMN cost_total_micros BIGINT NULL CHECK (cost_total_micros IS NULL OR cost_total_micros >= 0),
    ADD COLUMN cost_used_micros  BIGINT NOT NULL DEFAULT 0;

-- Answering "what did this cost, by whom, this month" without a scan.
CREATE INDEX usage_events_cost ON usage_events (at DESC) WHERE cost_micros IS NOT NULL;
