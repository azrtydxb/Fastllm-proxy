-- Constant headers every upstream request to a provider carries, beyond the
-- auth header and the protocol constants the proxy already builds.
--
-- The reason this exists: some endpoints decide things about a request from
-- headers the client used to supply and the proxy does not set — a pinned
-- `user-agent` above all. An operator who needs one had no way to say so:
-- the auth knobs cover the credential and nothing else. This is the
-- operator-configured escape hatch, applied after the built headers so it
-- wins, and it overrides the client's own header of the same name for the
-- same reason.
--
-- An object of name → value strings. Values are validated as header values
-- at the API surface; a value that is not a string in the database is
-- dropped with a log at snapshot build rather than failing the rebuild.

ALTER TABLE providers ADD COLUMN extra_headers jsonb NOT NULL DEFAULT '{}'::jsonb;
