-- A provider becomes a record.
--
-- Until now it was a grouping the UI invented at render time
-- (`web/src/views/Providers.jsx`: "a provider is a grouping this screen
-- invents, not a thing the API models"), derived by bucketing backends by
-- `api_base` origin. That works for a read-only screen and fails for
-- everything else: a service cannot register, heartbeat or expire a grouping,
-- and the 80 providers in `docs/providers.md` have nothing to attach to.
--
-- It also had a cost that was already visible. Six columns on `model_backends`
-- described the endpoint rather than the model -- `api_base`,
-- `upstream_api_key`, `protocol`, `auth_header`, `auth_scheme` and
-- `credential_kind` -- so one OpenRouter key was encrypted once per model and
-- rotating it was N writes, while the screen truthfully showed "credential
-- set" as a fact about the provider.
--
-- See .procoder/adr/0001-a-provider-model-belongs-to-exactly-one-provider.md.

CREATE TABLE providers (
    id       BIGSERIAL PRIMARY KEY,
    -- Display name and the handle a registration service uses. Derived from
    -- the endpoint's host:port on migration because that is what the UI was
    -- already calling a provider.
    name     TEXT NOT NULL UNIQUE,
    -- `static` and `cloud` are both human-created and never expire; the split
    -- exists so the UI can offer the catalogue for one and a free-text address
    -- for the other. `dynamic` is registered by a service under a lease and is
    -- the only kind anything is permitted to delete automatically.
    kind     TEXT NOT NULL DEFAULT 'static'
             CHECK (kind IN ('static', 'cloud', 'dynamic')),
    api_base TEXT NOT NULL,
    -- Moved verbatim from `model_backends`; the CHECK and the defaults are the
    -- ones migration 0013 chose, and the reasoning there still applies -- a
    -- nullable scheme so `Bearer`, another prefix, and no prefix at all are
    -- all expressible, because Gemini and Anthropic send the key raw.
    protocol         TEXT NOT NULL DEFAULT 'openai'
                     CHECK (protocol IN ('openai', 'anthropic', 'gemini')),
    auth_header      TEXT NOT NULL DEFAULT 'authorization',
    auth_scheme      TEXT NULL DEFAULT 'Bearer',
    -- Encrypted at rest and never returned by the admin API, exactly as it was
    -- on `model_backends` -- see the comment migration 0004 left there. It is
    -- still carried usably in the snapshot, so `/snapshot` must be TLS wherever
    -- providers have real credentials.
    upstream_api_key BYTEA,
    credential_kind  TEXT NOT NULL DEFAULT 'static'
                     CHECK (credential_kind IN ('static', 'gcp_service_account')),
    -- Which catalogue entry this came from, when it came from one. NULL for a
    -- hand-typed address. Nothing enforces it against a catalogue table yet;
    -- that arrives with the catalogue itself.
    catalogue_key    TEXT
);

COMMENT ON COLUMN providers.upstream_api_key IS
    'Encrypted with the key in FASTLLM_ENCRYPTION_KEY. Never returned by the '
    'admin API. Carried decrypted in the snapshot because the proxy has to '
    'present it upstream, so /snapshot must be TLS in any real deployment.';

-- One provider per distinct endpoint *and* credential. Two backends on the
-- same address with different keys are genuinely two providers: merging them
-- would silently give one model another model's credential.
INSERT INTO providers (name, kind, api_base, protocol, auth_header, auth_scheme,
                       upstream_api_key, credential_kind)
SELECT
    -- host:port, plus a counter only where that is ambiguous, so the common
    -- case reads the way the Providers screen already labelled it.
    CASE WHEN COUNT(*) OVER (PARTITION BY host) > 1
         THEN host || '#' || ROW_NUMBER() OVER (PARTITION BY host ORDER BY api_base, protocol)
         ELSE host
    END,
    -- Anything not on a private address is treated as hosted. This only sets
    -- the initial label; an operator can correct it, and nothing routes on it.
    CASE WHEN host ~ '^(192\.168\.|10\.|172\.(1[6-9]|2[0-9]|3[01])\.|127\.|localhost)'
         THEN 'static' ELSE 'cloud'
    END,
    api_base, protocol, auth_header, auth_scheme, upstream_api_key, credential_kind
FROM (
    SELECT DISTINCT
        COALESCE(SUBSTRING(api_base FROM '^[a-zA-Z][a-zA-Z0-9+.-]*://([^/]+)'), api_base) AS host,
        api_base, protocol, auth_header, auth_scheme, upstream_api_key, credential_kind
    FROM model_backends
) d;

-- The model side of the split. `upstream_model` and `default_max_tokens` are
-- the two columns on `model_backends` that really were about the model: what
-- the provider calls it, and the cap to supply when a request omits one.
ALTER TABLE models
  -- Nullable, and deliberately so. A model with no backend is representable
  -- today and does occur -- it is the drift this work exists to end -- so the
  -- migration preserves such rows rather than deleting them and cascading away
  -- their usage history. NULL means "no provider configured": not routable,
  -- and visible in the UI as needing attention.
  ADD COLUMN provider_id         BIGINT REFERENCES providers(id) ON DELETE CASCADE,
  ADD COLUMN upstream_model      TEXT,
  ADD COLUMN default_max_tokens  INTEGER
      CHECK (default_max_tokens IS NULL OR default_max_tokens > 0);

CREATE INDEX models_provider ON models(provider_id);

-- Each model takes its lowest-id backend. Models with a second backend are
-- handled below; taking the lowest id first keeps the original row pointing
-- where an operator would expect.
UPDATE models m
SET provider_id = p.id,
    upstream_model = b.upstream_model,
    default_max_tokens = b.default_max_tokens
FROM (
    SELECT DISTINCT ON (model_id) model_id, api_base, upstream_model,
           default_max_tokens, protocol, auth_header, auth_scheme,
           upstream_api_key, credential_kind
    FROM model_backends ORDER BY model_id, id
) b
JOIN providers p
  ON p.api_base = b.api_base
 AND p.protocol = b.protocol
 AND p.auth_header = b.auth_header
 AND p.auth_scheme IS NOT DISTINCT FROM b.auth_scheme
 AND p.upstream_api_key IS NOT DISTINCT FROM b.upstream_api_key
 AND p.credential_kind = b.credential_kind
WHERE m.id = b.model_id;

-- Names become unique per provider rather than globally.
--
-- This follows from the split rather than being chosen: two DGX Sparks both
-- serving `bge-m3` are two provider models, and both are called `bge-m3`
-- because the name is whatever the provider exposes. Frontend models are left
-- as the only global namespace, which is also the only namespace a client
-- names.
--
-- NULL `provider_id` rows do not conflict with each other under this
-- constraint. That is the correct reading: a model with no provider is not
-- routable, so two of them cannot be ambiguous.
ALTER TABLE models DROP CONSTRAINT models_name_key;
ALTER TABLE models ADD CONSTRAINT models_provider_name UNIQUE (provider_id, name);

-- A model with a second backend becomes a second provider model, and the two
-- are held together by a frontend model carrying the name clients already use.
--
-- Without this, every client calling a multi-backend model breaks: `bge-m3` is
-- one model with backends on .245 and .246 today, and callers name it
-- directly. The name is available for the frontend model because model names
-- just became provider-scoped.
CREATE TEMP TABLE split_models ON COMMIT DROP AS
SELECT b.model_id AS original_id, m.name, b.id AS backend_id, p.id AS provider_id,
       b.upstream_model, b.default_max_tokens,
       ROW_NUMBER() OVER (PARTITION BY b.model_id ORDER BY b.id) AS n
FROM model_backends b
JOIN models m ON m.id = b.model_id
JOIN providers p
  ON p.api_base = b.api_base
 AND p.protocol = b.protocol
 AND p.auth_header = b.auth_header
 AND p.auth_scheme IS NOT DISTINCT FROM b.auth_scheme
 AND p.upstream_api_key IS NOT DISTINCT FROM b.upstream_api_key
 AND p.credential_kind = b.credential_kind
WHERE b.model_id IN (SELECT model_id FROM model_backends GROUP BY model_id HAVING COUNT(*) > 1);

-- Rows 2..N become new models. Row 1 already updated the original above.
-- Price, context and cache settings are copied so the split is invisible in
-- reporting; `is_fallback` deliberately is not, since it is unique across the
-- table and only one row can hold it.
INSERT INTO models (name, description, input_price_per_mtok, output_price_per_mtok,
                    cache_ttl_seconds, context_length, policy,
                    provider_id, upstream_model, default_max_tokens)
SELECT m.name, m.description, m.input_price_per_mtok, m.output_price_per_mtok,
       m.cache_ttl_seconds, m.context_length, m.policy,
       s.provider_id, s.upstream_model, s.default_max_tokens
FROM split_models s JOIN models m ON m.id = s.original_id
WHERE s.n > 1;

INSERT INTO virtual_models (name, description)
SELECT DISTINCT m.name,
       'Balances ' || m.name || ' across the providers it was split over when '
       || 'providers became records (migration 0029).'
FROM split_models s JOIN models m ON m.id = s.original_id;

-- Equal weight, because the single model these came from had no way to express
-- a preference between its backends -- inventing one here would be a change of
-- behaviour disguised as a migration.
INSERT INTO virtual_model_defaults (virtual_model_id, model_id, weight, position)
SELECT v.id, m.id, 1, ROW_NUMBER() OVER (PARTITION BY v.id ORDER BY m.id) - 1
FROM virtual_models v
JOIN split_models s ON s.name = v.name
JOIN models m ON m.name = v.name AND m.provider_id = s.provider_id
GROUP BY v.id, m.id;

DROP TABLE model_backends;
