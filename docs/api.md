# API and administration

```bash
fastllm-proxy --config litellm_config.yaml --host 127.0.0.1 --port 4000
```

Point clients at it as an OpenAI endpoint:

```bash
curl http://localhost:4000/v1/chat/completions \
  -H 'Authorization: Bearer sk-...' \
  -H 'content-type: application/json' \
  -d '{"model":"Qwen/Qwen3-1.7B","stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

Reload after the model set changes — no restart, no dropped streams:

```bash
kill -HUP $(pgrep -x fastllm-proxy)
```

The machine-readable version of everything below is **[`openapi.json`](../openapi.json)**,
served by the running control plane at `GET /openapi.json` with Swagger UI at
`/docs`. It is checked against the router by `tests/openapi.rs` in both
directions — a route without a spec entry fails the build, and so does a spec
entry whose route no longer exists.

## In this section

| | |
|---|---|
| [The endpoints clients call](api/endpoints.md) | The proxied surface, what is not proxied, the response cache, rate-limit headers and retries |
| [Admin API](api/admin.md) | Models, backends, keys, principals, prices, live health, and the audit log |
| [Routing rules](api/routing-rules.md) | The rule grammar, and the dry-run that answers which rule would decide |
| [Authentication, sessions and TLS](api/auth.md) | Sessions, per-route permissions, encryption at rest, and which listener must be TLS |
| [The control-plane protocol](api/control-plane.md) | `/usage`, `/health-report`, budgets and rate-limit reconciliation |

Provider base URLs and the translation limits moved to
[Providers](providers.md). Every flag is in the
[command-line reference](cli.md).
