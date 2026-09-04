---
name: fastllm-auth
description: Authenticate to the FastLLM control plane and manage sessions — log in for a session cookie, log out, set or reset a principal's password, and revoke every session at once. Use when an admin call returns 401 or "no session cookie", when setting up admin access, or after a suspected credential leak. Not for gateway API keys used by clients (fastllm-principals).
---

# FastLLM auth

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
| `PUT` | `/admin/principals/{id}/password` | Set principals id password | `password` |
| `POST` | `/admin/sessions/revoke-all` | Invalidate every session, including the caller's | — |
| `POST` | `/login` | Exchange a name and password for a session cookie | `name`, `password` |
| `POST` | `/logout` | Clear the session cookie | — |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**Two credential systems, deliberately different.** Admin access is a
name+password login returning a session cookie; client access to `/v1/*` is a
principal API key sent as a bearer token. Passwords hash with Argon2id, API keys
with SHA-256 — keys are high-entropy random, passwords are low-entropy and
human-chosen. Do not conflate them.

**A `master-key` secret in the cluster is a gateway key, not an admin login.**
Presenting it to `/admin/*` returns `{"error":"no session cookie"}`.

**`revoke-all` logs out every session including your own.** Expect to log in
again immediately afterwards.
