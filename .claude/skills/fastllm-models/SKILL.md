---
name: fastllm-models
description: Register and maintain the models FastLLM can serve — create, patch or delete a model, attach backends to it, remove a backend, and set the deployment-wide fallback model. Use when adding a new inference endpoint, pointing a model at a different host or port, retiring a backend, or choosing what catches a request when every other target fails. Not for choosing between models per request (fastllm-routing).
---

# FastLLM models

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
| `DELETE` | `/admin/backends/{id}` | Delete backends id | — |
| `GET` | `/admin/fallback-model` | Read fallback-model | — |
| `PUT` | `/admin/fallback-model` | Set fallback-model | `model_id`* |
| `GET` | `/admin/models` | Read models | — |
| `POST` | `/admin/models` | Create models | `name`, `description`*, `unpriced`, `input_price_per_mtok`*, `output_price_per_mtok`*, `default`, `cache_ttl_seconds`*, `context_length`*, `backends`, `policy`* |
| `PATCH` | `/admin/models/{id}` | Correct a model in place. An explicit null clears a field; an absent field is left alone | `description`*, `input_price_per_mtok`*, `output_price_per_mtok`*, `cache_ttl_seconds`*, `context_length`*, `policy`* |
| `DELETE` | `/admin/models/{id}` | Delete models id | — |
| `POST` | `/admin/models/{id}/backends` | Create models id backends | `api_base`, `upstream_model`*, `upstream_api_key`*, `Authorization`, `protocol`*, `auth_header`*, `auth_scheme`*, `default_max_tokens`*, `credential_kind`* |
| `GET` | `/admin/providers` | Read providers | — |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**A model and a virtual model cannot share a name** — 409, because a client
request naming it would be ambiguous. Mirrored in both create paths.

**The fallback model is appended to every chain, virtual or concrete.** It is the
last resort when a rule author could not anticipate a failure mode; it is skipped
when already present in the chain, so naming it explicitly does not double it.

**A backend that fails health checks leaves rotation but is not dropped from the
chain.** When nothing is healthy the request still goes somewhere and the real
upstream error reaches the client, which beats a synthetic 503.
