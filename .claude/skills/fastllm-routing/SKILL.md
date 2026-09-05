---
name: fastllm-routing
description: Route requests across models in FastLLM — create, inspect or change frontend models, their default targets, weighted splits, and routing rules (by caller, prompt size, streaming, headers, budget, time of day, semantic class, or backend load for local/cloud spillover). Use when asked to expose a model under a client-facing name, decide which backend a request should hit, set up failover or canary traffic, or explain why a request went where it did. Not for registering models or backends (fastllm-models) or for sending inference requests (fastllm-gateway).
---

# FastLLM routing

A **frontend model** is a client-facing name backed by an ordered list of rules.
The first rule whose conditions all hold wins and commits to its own targets;
if none match, the defaults are used. Everything is pre-resolved into the
snapshot, so the request path does no I/O to route.

## Auth

Admin endpoints need a **session cookie**, not a bearer token. The gateway
master key is not an admin credential.

```bash
curl -sk -c /tmp/ck -X POST https://192.168.10.129:4001/login \
  -H 'content-type: application/json' -d '{"name":"<user>","password":"<pw>"}'
curl -sk -b /tmp/ck https://192.168.10.129:4001/admin/frontend-models
```

## Test before you apply

`POST /admin/routing/dry-run` answers "which model would this request hit, and
which rule decided" without changing anything. Use it before and after any
change here — it is the only way to check a rule does what you meant.

<!-- BEGIN GENERATED: endpoints -->

| Method | Path | Summary | Body fields |
|---|---|---|---|
| `DELETE` | `/admin/frontend-model-defaults/{id}` | Delete frontend-model-defaults id | — |
| `GET` | `/admin/frontend-models` | Read frontend-models | — |
| `POST` | `/admin/frontend-models` | Create frontend-models | `name`, `description`*, `targets`, `policy`* |
| `PATCH` | `/admin/frontend-models/{id}` | Change how a frontend model chooses between its targets | `name`*, `policy`* |
| `DELETE` | `/admin/frontend-models/{id}` | Delete frontend-models id | — |
| `POST` | `/admin/frontend-models/{id}/defaults` | Create frontend-models id defaults | `provider_model_id`, `weight`*, `position` |
| `POST` | `/admin/frontend-models/{id}/rules` | Create frontend-models id rules | `position`, `match_condition` |
| `POST` | `/admin/routing/dry-run` | Which rule would decide, and what the chain resolves to, without dispatching | `model`, `principal_id`*, `streaming`*, `prompt_tokens`*, `max_tokens`*, `headers`*, `class`*, `class_refines`* |
| `DELETE` | `/admin/rule-targets/{id}` | Delete rule-targets id | — |
| `DELETE` | `/admin/rules/{id}` | Delete rules id | — |
| `POST` | `/admin/rules/{id}/targets` | Create rules id targets | `provider_model_id`, `weight`*, `position` |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**A provider model and a frontend model may share a name, and normally do.**
This used to be a 409. It is not ambiguous: `resolve_target_models` looks in
frontend models first and falls through to a provider model only when there is
none of that name, so the frontend model wins deterministically. Migration 0034
depends on it — every provider model gets a frontend model of the same name, so
it stays callable once frontend models are the only addressable surface.
Renaming the provider model out of the way instead would revoke every grant
naming it. Pinned by `a_provider_model_and_a_frontend_model_may_share_a_name`.

**`frontend_model_defaults.position` is `NOT NULL` with no default.** An INSERT
that omits it fails. The API sets it; hand-written SQL must too.

**`weight` is a relative share, not a percentage.** Two targets at 1 and 1 split
evenly; 1 and 3 split 25/75. They need not sum to 100, so adding a third target
never forces you to rebalance the other two.

**First match wins, and a matching rule commits.** If its targets resolve to
nothing routable the request fails — it does not fall through to the next rule
or to the defaults. Falling through would make "first match wins" a lie that
depends on backend health the rule author cannot see.

**The weighted split is deterministic, not random.** It hashes the same request
prefix the backend router hashes, so a multi-turn conversation stays on one side
of a canary instead of flipping per request.

**`max_inflight_per_backend` is the only condition that is not a pure function
of the request.** It reads live in-flight counters, so two identical requests a
second apart can route differently. That is the price of local/cloud spillover;
the field is named for the mechanism rather than the intent for that reason.

## Prefer the API over SQL

Changes made through `/admin/*` write an audit row and rebuild the snapshot.
Direct `psql` writes do neither — the change still reaches proxies on their next
snapshot poll, but nothing records who made it or why. If you must use SQL
because no admin credential is available, say so explicitly in your report.

## Verify

```bash
# what the control plane now believes
curl -sk -b /tmp/ck https://192.168.10.129:4001/admin/frontend-models

# what a request would actually do
curl -sk -b /tmp/ck -X POST https://192.168.10.129:4001/admin/routing/dry-run \
  -H 'content-type: application/json' -d '{"model":"<virtual>","prompt_tokens":100}'
```

Changes reach the proxies on their snapshot poll, not instantly. A gateway
`401` means the proxy is healthy and rejecting an unauthenticated request; a
`/health` `503` means it has no usable snapshot yet.
