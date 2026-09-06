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
| `PUT` | `/admin/fallback-model` | Set fallback-model | `provider_model_id`* |
| `GET` | `/admin/provider-catalogue` | Known providers and how to reach them | — |
| `GET` | `/admin/provider-models` | Read provider models | — |
| `POST` | `/admin/provider-models` | Create provider models | `name`, `description`*, `unpriced`, `input_price_per_mtok`*, `output_price_per_mtok`*, `default`, `cache_ttl_seconds`*, `context_length`* |
| `PATCH` | `/admin/provider-models/{id}` | Correct a model in place. An explicit null clears a field; an absent field is left alone | `name`*, `description`*, `input_price_per_mtok`*, `output_price_per_mtok`*, `cache_ttl_seconds`*, `context_length`* |
| `DELETE` | `/admin/provider-models/{id}` | Delete models id | — |
| `POST` | `/admin/provider-models/{id}/backends` | Create models id backends | `provider_id`*, `api_base`*, `upstream_model`*, `upstream_api_key`*, `Authorization`, `protocol`*, `auth_header`*, `auth_scheme`*, `default_max_tokens`*, `credential_kind`* |
| `GET` | `/admin/providers` | Read providers | — |
| `POST` | `/admin/providers` | Add a provider: an endpoint and the credential that reaches it | `name`*, `kind`*, `catalogue_key`*, `api_base`*, `protocol`*, `auth_header`*, `auth_scheme`*, `upstream_api_key`*, `credential_kind`*, `skip_validation`* |
| `POST` | `/admin/providers/register` | Register or refresh a provider's lease | `api_base`, `node`, `name`*, `engine`*, `ttl_seconds` |
| `PATCH` | `/admin/providers/{id}` | Rename a provider, move it, or rotate its credential. An absent upstream_api_key leaves the stored one alone; "" clears it | `name`*, `kind`*, `api_base`*, `protocol`*, `auth_header`*, `auth_scheme`*, `upstream_api_key`*, `credential_kind`*, `skip_validation`* |
| `DELETE` | `/admin/providers/{id}` | Delete a provider that serves no models | — |
| `GET` | `/admin/providers/{id}/available-models` | What a provider is currently serving | — |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**A provider model and a frontend model may share a name, and normally do.**
The frontend model wins during resolution, and migration 0034 gives every
provider model one of the same name so it stays callable. This used to be a
409 in both create paths; it no longer is.

**The fallback model catches a frontend model whose chain ran out.** It is the
last resort when a rule author could not anticipate a failure mode; it is skipped
when already present in the chain, so naming it explicitly does not double it.

**A backend that fails health checks leaves rotation but is not dropped from the
chain.** When nothing is healthy the request still goes somewhere and the real
upstream error reaches the client, which beats a synthetic 503.

**A provider is created before its models, not by them.** `POST
/admin/providers` takes the endpoint and its credential — from a catalogue key
for a cloud vendor, or a typed `api_base` for anything else. Attaching a model
then only has to name it:

```bash
# The endpoint and its key, once.
curl -sk -b /tmp/ck -X POST https://192.168.10.129:4001/admin/providers \
  -H 'content-type: application/json' \
  -d '{"catalogue_key":"anthropic","upstream_api_key":"sk-ant-..."}'

# What that provider is actually serving, before deciding what to register.
curl -sk -b /tmp/ck https://192.168.10.129:4001/admin/providers/7/available-models

# The model, on that provider.
curl -sk -b /tmp/ck -X POST \
  https://192.168.10.129:4001/admin/provider-models/42/backends \
  -H 'content-type: application/json' \
  -d '{"provider_id":7,"upstream_model":"claude-sonnet-4-5"}'
```

`POST .../backends` with an `api_base` instead still works and still
find-or-creates a provider — that is how every backend was attached before
providers were records, and every existing script does it that way.

**`provider_id` and the fields describing an endpoint are mutually exclusive.**
Sending `upstream_api_key` alongside a `provider_id` is a 400, not a silent
preference for one source: the caller would otherwise believe they had set a
credential while the provider's is what actually gets sent. Change those with
`PATCH /admin/providers/{id}`, which rotates the key for every model on it in
one write.

**A catalogue `base_url` can contain a `<placeholder>`.** Bedrock and Vertex
both encode a region, and Vertex a project. `POST /admin/providers` refuses an
address that still has one in it rather than storing something that resolves
nowhere and then reports itself unreachable.
