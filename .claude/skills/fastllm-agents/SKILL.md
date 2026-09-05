---
name: fastllm-agents
description: Manage and invoke A2A agents behind FastLLM — register, patch, delete and list agents on the control plane, list them through the gateway, fetch an agent card, and invoke an agent by name. Use when putting one address in front of several agents, controlling which keys may set an agent running, or debugging an agent invocation.
---

# FastLLM agents

## Auth

Admin endpoints need a **session cookie**, not a bearer token — the gateway
master key is not an admin credential.

```bash
curl -sk -c /tmp/ck -X POST https://192.168.10.129:4001/login \
  -H 'content-type: application/json' -d '{"name":"<user>","password":"<pw>"}'
curl -sk -b /tmp/ck https://192.168.10.129:4001/admin/...
```

<!-- BEGIN GENERATED: endpoints -->

| Method | Path | Summary | Body fields |
|---|---|---|---|
| `GET` | `/admin/a2a-agents` | List A2A agents. Reports whether a credential is set, never what it is | — |
| `POST` | `/admin/a2a-agents` | Register an A2A agent | `name`, `url`, `description`*, `protocol_version`*, `auth_header`*, `auth_scheme`*, `upstream_api_key`*, `enabled`* |
| `PATCH` | `/admin/a2a-agents/{id}` | Update an A2A agent | `name`*, `url`*, `description`*, `protocol_version`*, `enabled`*, `upstream_api_key`* |
| `DELETE` | `/admin/a2a-agents/{id}` | Delete an A2A agent | — |
| `GET` | `/v1/agents` | A2A agents this key may invoke | — |
| `POST` | `/v1/agents/{name}` | Every A2A JSON-RPC method on one path. Only the methods this gateway forwards are accepted | — |
| `GET` | `/v1/agents/{name}/.well-known/agent-card.json` | The agent's card, with `url` rewritten to this gateway so the next call is still authorised and attributed | — |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**An agent acts** — it runs, calls tools and spends money — so which key may
invoke which agent is an access question, not a routing one.

**`protocol_version` is pinned per agent.** An agent speaking a different A2A
version than the one registered will not behave as its card advertises.
