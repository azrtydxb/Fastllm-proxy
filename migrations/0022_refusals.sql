-- Requests the gateway refused itself, alongside the ones a backend answered.
--
-- Until now `usage_events` held only requests that reached a backend, because
-- the row is written by the body that forwards the response and a refused
-- request has no such body. That left the GUI unable to show, from Postgres
-- alone, the failures operators are most often asked about: a key without a
-- grant, a principal over its rate limit or budget, and — the case that forced
-- this — every backend being unreachable.
--
-- That last one is why a nullable column was not enough on its own. With no
-- backend reachable, no response comes back, so nothing was recorded, so an
-- error chart drawn from this table showed a flat zero during a total outage.
-- A monitoring view whose worst failure mode is "looks healthy while nothing
-- works" is worse than no view.
--
-- NULL means "a backend answered this request, whatever its status" — the
-- shape every pre-existing row has, which is why no backfill is needed and
-- none is done. A non-NULL value names why the gateway stopped it:
--
--   authorisation  403, authenticated but not granted the model
--   rate_limit     429, over a configured per-minute limit
--   budget         402, budget window exhausted
--   no_backend     502, nothing in the chain could be reached
--
-- Deliberately absent: unauthenticated 401s. There is no principal to
-- attribute them to, and `usage_events.principal_id` is not nullable — but the
-- stronger reason is that 401 is the one refusal an anonymous caller can
-- trigger at will, and recording it would let unauthenticated traffic drive
-- unbounded writes here. That count stays in `/metrics`.
--
-- Text rather than an enum type: the set will grow (a future circuit breaker,
-- a request-size ceiling), and adding a value to a Postgres enum is a schema
-- migration coordinated with a deploy, while adding one here is not. The cost
-- is that the database does not enforce the vocabulary; `usage::Refusal` does,
-- on the only path that writes it.
ALTER TABLE usage_events
    ADD COLUMN refusal text;

-- The refusal charts scan a window and group by kind, and refusals are the
-- rare rows: a partial index keeps that scan proportional to how much was
-- refused rather than to how much was served.
CREATE INDEX usage_events_refusal_at ON usage_events (at DESC, refusal)
    WHERE refusal IS NOT NULL;
