-- Which credential kinds a catalogue entry can actually use.
--
-- `credential_kind` has two values and exactly one provider needs the second:
-- Vertex AI, which cannot use a static secret because its access token is
-- minted from a service-account key file and expires. Every other entry, and
-- every self-hosted endpoint, takes a static key.
--
-- Until now the UI offered the choice to everyone, which put a Google-shaped
-- question on the form for people adding Groq. The alternative was to hardcode
-- the string `vertex` in the JSX, and that would duplicate here-knowledge in a
-- place no query can reach: whether a vendor can take a service account is a
-- fact about the vendor, so the catalogue is where it belongs.
--
-- A comma-separated list rather than a table: it has one row's worth of
-- variation across fourteen entries, and `credential_kind` is itself a CHECK
-- constraint rather than a lookup table, so a join here would model the
-- exception more heavily than the rule.

ALTER TABLE provider_catalogue
    ADD COLUMN credential_kinds TEXT NOT NULL DEFAULT 'static';

COMMENT ON COLUMN provider_catalogue.credential_kinds IS
    'Comma-separated credential_kind values this entry accepts. The first is '
    'the default. Everything is static except Vertex AI, whose token is minted '
    'from a service-account key file.';

UPDATE provider_catalogue
   SET credential_kinds = 'gcp_service_account,static'
 WHERE key = 'vertex';
