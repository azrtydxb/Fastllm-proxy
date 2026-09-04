---
name: fastllm-mcp
description: Manage and use MCP servers behind FastLLM — register, patch, delete and list MCP servers on the control plane, and list or call their tools through the gateway. Use when putting one address in front of several MCP servers, granting a client access to a tool, or debugging an MCP tool call.
---

# FastLLM mcp

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
| `GET` | `/admin/mcp-servers` | List MCP servers. Reports whether a credential is set, never what it is | — |
| `POST` | `/admin/mcp-servers` | Register an MCP server | `name`, `url`, `transport`*, `description`*, `auth_header`*, `auth_scheme`*, `upstream_api_key`*, `enabled`* |
| `PATCH` | `/admin/mcp-servers/{id}` | Update an MCP server. An absent upstream_api_key leaves the stored credential alone; "" clears it | `url`*, `description`*, `enabled`*, `upstream_api_key`* |
| `DELETE` | `/admin/mcp-servers/{id}` | Delete an MCP server | — |
| `GET` | `/v1/mcp/servers` | MCP servers this key may invoke | — |
| `POST` | `/v1/mcp/tools/call` | Invoke one namespaced tool. {"name": "<server>__<tool>", "arguments": {}} | — |
| `POST` | `/v1/mcp/tools/list` | Every tool across every server this key may invoke, namespaced <server>__<tool>. `unreachable` names servers that did not answer, so a missing tool is diagnosable | — |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**Two surfaces, two credentials.** `/admin/mcp-servers` needs the admin session
cookie; `/v1/mcp/*` needs a principal API key.

**Tool listing goes through the gateway, not the control plane** — `/v1/mcp/tools/list`
reflects what the calling principal is allowed to see.
