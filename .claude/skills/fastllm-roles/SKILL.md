---
name: fastllm-roles
description: Define FastLLM roles and their permissions — create or delete roles, grant and revoke individual permissions, and inspect what a role allows. Use when deciding what a group of callers may do, setting up least-privilege access, or diagnosing a 403 from an admin or gateway call. Not for attaching roles to a specific principal (fastllm-principals).
---

# FastLLM roles

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
| `GET` | `/admin/roles` | Read roles | — |
| `POST` | `/admin/roles` | Create roles | `name`, `description`* |
| `PATCH` | `/admin/roles/{name}` | Rename a role, or change its description | `name`*, `description`* |
| `DELETE` | `/admin/roles/{name}` | Delete roles name | — |
| `POST` | `/admin/roles/{name}/permissions` | Create roles name permissions | `verb`, `resource`* |
| `DELETE` | `/admin/roles/{name}/permissions` | Delete roles name permissions | `verb`, `resource`* |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**Grants are flattened into the snapshot.** Authorisation on the request path is
a set lookup, not a graph walk — so a role change takes effect only once the
proxies pick up the next snapshot.

**A caller is authorised against the name it used, which is a frontend model's.**
Provider models are inventory and cannot be named by a client at all, so grants
are written on frontend models. A grant on one covers the chain it routes to —
adding a target therefore extends the reach of everyone holding it, and editing
a frontend model needs `config:write`.
