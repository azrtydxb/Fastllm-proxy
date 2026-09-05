---
name: fastllm-principals
description: Manage FastLLM principals (API clients and users) and their API keys — create or delete principals, issue and revoke keys, attach roles, and set per-principal budgets and rate limits. Use when adding a new client or teammate, rotating a key, granting someone access to a model, or capping what one caller may spend. Not for role definitions and permissions (fastllm-roles) or global limits (fastllm-limits-budgets).
---

# FastLLM principals

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
| `GET` | `/admin/keys` | Read keys | — |
| `POST` | `/admin/keys` | Mint a key. The plaintext is returned once and never again | `name`, `principal_id`, `expires_at`* |
| `DELETE` | `/admin/keys/{id}` | Revoke a key. The row stays for audit | — |
| `GET` | `/admin/principals` | Read principals | — |
| `POST` | `/admin/principals` | Create principals | `name`, `kind`*, `email`* |
| `PATCH` | `/admin/principals/{id}` | Rename a principal, or correct its email | `name`*, `email`* |
| `DELETE` | `/admin/principals/{id}` | Delete principals id | — |
| `PUT` | `/admin/principals/{id}/budget` | Set principals id budget | `tokens_total`*, `cost_total_micros`*, `window` |
| `DELETE` | `/admin/principals/{id}/budget` | Delete principals id budget | — |
| `PUT` | `/admin/principals/{id}/limits` | Set principals id limits | `requests_per_min`*, `tokens_per_min`* |
| `DELETE` | `/admin/principals/{id}/limits` | Delete principals id limits | — |
| `PUT` | `/admin/principals/{id}/password` | Set principals id password | `password` |
| `POST` | `/admin/principals/{id}/roles` | Create principals id roles | `role` |
| `DELETE` | `/admin/principals/{id}/roles/{role}` | Delete principals id roles role | — |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**API keys hash with SHA-256 and cannot be read back.** Capture the key at
creation; there is no endpoint that returns it later. If it is lost, issue a new
one and delete the old.

**A principal's budget and limits are separate sub-resources** (`/budget`,
`/limits`), each with its own `PUT` and `DELETE`. Deleting the principal is not
the same as clearing them.

**Deleting a principal orphans its usage rows.** `usage_events` is keyed on
principal id, and rows whose principal no longer resolves are dropped at ingest
— historical spend for that caller stops being answerable.
