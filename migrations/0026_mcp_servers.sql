-- MCP servers, proxied the way models are.
--
-- The shape deliberately mirrors `model_backends`: an address, a credential
-- encrypted at rest, and the two auth knobs, because an MCP server is
-- authenticated exactly like any other upstream and inventing a second
-- credential mechanism for it would mean two things to rotate and two things
-- to get wrong.
--
-- What is absent, and on purpose: `stdio`. LiteLLM supports launching an MCP
-- server as a child process with injected environment. A gateway that spawns
-- processes on behalf of a request is a different trust boundary from one that
-- forwards HTTP, and the proxy runs with a read-only root filesystem and no
-- shell for good reasons. A stdio server belongs behind an HTTP transport that
-- someone else operates.
CREATE TABLE mcp_servers (
    id BIGSERIAL PRIMARY KEY,
    -- The name callers address it by, and the namespace its tools appear
    -- under. Kept URL-safe so it can sit in a path segment.
    name TEXT NOT NULL UNIQUE CHECK (name ~ '^[a-zA-Z0-9][a-zA-Z0-9_-]*$'),
    url TEXT NOT NULL,
    -- `http` is MCP's streamable HTTP transport; `sse` is the older one that
    -- several published servers still speak.
    transport TEXT NOT NULL DEFAULT 'http'
        CHECK (transport IN ('http', 'sse')),
    description TEXT NOT NULL DEFAULT '',
    -- Same defaults and same meaning as model_backends: absent scheme sends
    -- the credential raw, which is what several MCP hosts want.
    auth_header TEXT NOT NULL DEFAULT 'authorization',
    auth_scheme TEXT NULL DEFAULT 'Bearer',
    -- AES-256-GCM, encrypted before it reaches this table, exactly like
    -- model_backends.upstream_api_key. Never read back through the API.
    upstream_api_key BYTEA,
    -- A server that is failing, or one being introduced, without deleting the
    -- row and losing its grants.
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Authorisation reuses the permissions table rather than growing a parallel
-- one: `mcp:invoke` on `mcp/*` or on `mcp/github` is the same machinery as
-- `model:invoke` on `model/*`, so a role that already means something to an
-- operator keeps meaning it.
INSERT INTO permissions (verb, resource) VALUES ('mcp:invoke', 'mcp/*');

-- `admin` holds every permission, and that was expressed as a one-off INSERT
-- in 0001 rather than as a rule, so a permission added later has to be granted
-- here or `admin` silently stops being full access.
INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id FROM roles r, permissions p
WHERE r.name = 'admin' AND p.verb = 'mcp:invoke'
ON CONFLICT DO NOTHING;

-- `inference` deliberately does not get it. A key that may invoke models is
-- not, by that fact, a key that may reach every tool server the deployment
-- knows about — tools have side effects and models do not.
