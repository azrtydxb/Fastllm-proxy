-- A2A agents, proxied the way MCP servers are.
--
-- Same shape for the same reason: an address, a credential encrypted at rest,
-- and the two auth knobs. An agent is an upstream that authenticates like any
-- other, and a third credential mechanism would be a third thing to rotate.
CREATE TABLE a2a_agents (
    id BIGSERIAL PRIMARY KEY,
    -- Addressed by name in a URL path, so constrained the same way an MCP
    -- server's is.
    name TEXT NOT NULL UNIQUE CHECK (name ~ '^[a-zA-Z0-9][a-zA-Z0-9_-]*$'),
    url TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    -- Pinned rather than inferred. A2A 0.3 discriminates objects by `kind`;
    -- 1.0 uses protobuf JSON envelopes with PascalCase method names. Guessing
    -- from the shape of a client's request means an agent card that says one
    -- thing and a response that is the other, which is worse than either.
    protocol_version TEXT NOT NULL DEFAULT '0.3'
        CHECK (protocol_version IN ('0.3', '1.0')),
    auth_header TEXT NOT NULL DEFAULT 'authorization',
    auth_scheme TEXT NULL DEFAULT 'Bearer',
    upstream_api_key BYTEA,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Same machinery again: `agent:invoke` on `agent/*` or `agent/<name>`.
INSERT INTO permissions (verb, resource) VALUES ('agent:invoke', 'agent/*');

INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id FROM roles r, permissions p
WHERE r.name = 'admin' AND p.verb = 'agent:invoke'
ON CONFLICT DO NOTHING;

-- `inference` does not get it, for the reason `mcp:invoke` does not: an agent
-- acts. A key that may invoke models is not, by that fact, a key that may set
-- an agent running against whatever it is wired to.
