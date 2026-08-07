-- How to interpret `upstream_api_key`.
--
-- Every provider but one is reachable with a static secret: the column holds
-- the key, and the proxy presents it. Google Vertex AI is the exception — its
-- OpenAI-compatible endpoint wants an OAuth2 access token, and those expire
-- hourly, so no static value can stand in for one.
--
-- 'gcp_service_account' means the column holds a service-account *key file*
-- rather than a credential the proxy can use directly. The control plane
-- exchanges it for an access token while building the snapshot, and ships the
-- token. The data plane is unaware of the difference: it receives an ordinary
-- bearer credential either way, and does no I/O to obtain one.
--
-- Defaulted rather than backfilled per row: every existing backend is static
-- by definition, since this is the first alternative to exist.
ALTER TABLE model_backends
    ADD COLUMN credential_kind TEXT NOT NULL DEFAULT 'static'
        CHECK (credential_kind IN ('static', 'gcp_service_account'));
