-- What a dynamically registered provider needs beyond an address.
--
-- A host that dies cannot send "remove me", so absence has to be the signal.
-- These columns are that signal: a lease the registering service refreshes, and
-- the timestamps that let expiry be *staged* rather than immediate.
--
-- Staged, because deleting on the first failed probe is wrong here. A 27B load
-- on a DGX Spark takes over ten minutes during which the endpoint does not
-- answer, and a host reboot is routine. Deleting a provider throws away its
-- credential and, before migration 0031, would have taken its usage history
-- with it. Suppressing routing is reversible; deletion is not. See
-- .procoder/adr/0004-dynamic-providers-degrade-before-they-are-deleted.md.

ALTER TABLE providers
  -- Which host registered this, so a compromised node can be scoped to its own
  -- providers and an operator can see where a provider came from.
  ADD COLUMN node               TEXT,
  -- Engine hint, for metadata only. Never load-bearing: every engine answers
  -- `GET /v1/models`, which is the whole of what registration needs, so an
  -- unrecognised engine degrades to "no metadata" and not to "unsupported".
  ADD COLUMN engine             TEXT,
  -- NULL for static and cloud providers: they are here because a human put
  -- them here, and absence is not evidence the human changed their mind. Only
  -- a provider with a lease can expire.
  ADD COLUMN lease_expires_at   TIMESTAMPTZ,
  -- Last successful `GET /v1/models` against this provider, by the control
  -- plane. Distinct from the lease: the lease says the agent is alive, this
  -- says the endpoint is.
  ADD COLUMN last_seen_at       TIMESTAMPTZ,
  -- Set when the provider stops answering or its lease lapses, cleared when it
  -- recovers. Deletion is a function of how long this has been set, so
  -- recovery costs nothing and a long outage is still visible as one.
  ADD COLUMN degraded_since     TIMESTAMPTZ,
  -- Why it is degraded, for an operator reading the screen: "unreachable" and
  -- "serving something else" are different problems with different fixes, and
  -- the second is the one no liveness probe can report.
  ADD COLUMN degraded_reason    TEXT;

CREATE INDEX providers_lease ON providers(lease_expires_at)
    WHERE lease_expires_at IS NOT NULL;

COMMENT ON COLUMN providers.lease_expires_at IS
    'When this provider stops being vouched for. NULL means no lease: static '
    'and cloud providers never expire. Only kind = dynamic carries one.';
