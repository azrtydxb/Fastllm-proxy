-- A principal is whatever permissions attach to: a human who signs in, or a
-- service account that owns API keys. Roles attach to principals and never to
-- individual keys, so rotating a key cannot silently change its reach.
CREATE TABLE principals (
    id            BIGSERIAL PRIMARY KEY,
    kind          TEXT NOT NULL CHECK (kind IN ('user', 'service_account')),
    name          TEXT NOT NULL UNIQUE,
    email         TEXT UNIQUE,
    -- Argon2id, and only for kind='user'. Passwords are low-entropy and
    -- human-chosen, which is the case a slow KDF exists for.
    password_hash TEXT,
    disabled      BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE roles (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE permissions (
    id       BIGSERIAL PRIMARY KEY,
    verb     TEXT NOT NULL,
    resource TEXT NOT NULL,
    UNIQUE (verb, resource)
);

CREATE TABLE role_permissions (
    role_id       BIGINT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    permission_id BIGINT NOT NULL REFERENCES permissions(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, permission_id)
);

CREATE TABLE principal_roles (
    principal_id BIGINT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    role_id      BIGINT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    PRIMARY KEY (principal_id, role_id)
);

CREATE TABLE api_keys (
    id           BIGSERIAL PRIMARY KEY,
    -- SHA-256 of the key. Plaintext is shown once at creation and never stored.
    hash         BYTEA NOT NULL UNIQUE,
    -- First few characters, for display only.
    prefix       TEXT NOT NULL,
    name         TEXT NOT NULL,
    principal_id BIGINT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    expires_at   TIMESTAMPTZ,
    disabled     BOOLEAN NOT NULL DEFAULT FALSE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ
);
CREATE INDEX api_keys_principal ON api_keys(principal_id);

CREATE TABLE models (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE model_backends (
    id               BIGSERIAL PRIMARY KEY,
    model_id         BIGINT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    api_base         TEXT NOT NULL,
    upstream_model   TEXT NOT NULL,
    -- Encrypted at rest and never returned by the admin API, but it IS carried
    -- usably in the snapshot: the proxy has to present it to the backend, so
    -- unlike a user API key it cannot be reduced to a hash. /snapshot must be
    -- TLS wherever backends have real credentials.
    upstream_api_key BYTEA
);
CREATE INDEX model_backends_model ON model_backends(model_id);

-- Grants are expressed as permissions on resources, so 'model:invoke' on
-- 'model/*' and on 'model/qwen3' use the same machinery.
INSERT INTO permissions (verb, resource) VALUES
    ('model:invoke', 'model/*'),
    ('key:create',   '*'),
    ('key:revoke',   '*'),
    ('config:write', '*'),
    ('usage:read',   '*');

INSERT INTO roles (name, description) VALUES
    ('admin',    'Full administrative access'),
    ('operator', 'Manage models and keys, no user administration'),
    ('inference','Invoke any model, no administrative access');

INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id FROM roles r, permissions p WHERE r.name = 'admin';

INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id FROM roles r, permissions p
WHERE r.name = 'inference' AND p.verb = 'model:invoke';
