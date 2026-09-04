---
name: fastllm-routing
description: Route requests across models in FastLLM — create, inspect or change virtual models, their default targets, weighted splits, and routing rules (by caller, prompt size, streaming, headers, budget, time of day, semantic class, or backend load for local/cloud spillover). Use when asked to expose a model under a client-facing name, decide which backend a request should hit, set up failover or canary traffic, or explain why a request went where it did. Not for registering models or backends (fastllm-models) or for sending inference requests (fastllm-gateway).
---

# FastLLM routing

A **virtual model** is a client-facing name backed by an ordered list of rules.
The first rule whose conditions all hold wins and commits to its own targets;
if none match, the defaults are used. Everything is pre-resolved into the
snapshot, so the request path does no I/O to route.

## Auth

Admin endpoints need a **session cookie**, not a bearer token. The gateway
master key is not an admin credential.

```bash
curl -sk -c /tmp/ck -X POST https://192.168.10.129:4001/login \
  -H 'content-type: application/json' -d '{"name":"<user>","password":"<pw>"}'
curl -sk -b /tmp/ck https://192.168.10.129:4001/admin/virtual-models
```

## Test before you apply

`POST /admin/routing/dry-run` answers "which model would this request hit, and
which rule decided" without changing anything. Use it before and after any
change here — it is the only way to check a rule does what you meant.

<!-- BEGIN GENERATED: endpoints -->

| Method | Path | Summary | Body fields |
|---|---|---|---|
| `POST` | `/admin/routing/dry-run` | Which rule would decide, and what the chain resolves to, without dispatching | `model`, `principal_id`*, `streaming`*, `prompt_tokens`*, `max_tokens`*, `headers`*, `class`*, `class_refines`* |
| `DELETE` | `/admin/rule-targets/{id}` | Delete rule-targets id | — |
| `DELETE` | `/admin/rules/{id}` | Delete rules id | — |
| `POST` | `/admin/rules/{id}/targets` | Create rules id targets | `model_id`, `weight`*, `position` |
| `DELETE` | `/admin/virtual-model-defaults/{id}` | Delete virtual-model-defaults id | — |
| `GET` | `/admin/virtual-models` | Read virtual-models | — |
| `POST` | `/admin/virtual-models` | Create virtual-models | `name`, `description`* |
| `DELETE` | `/admin/virtual-models/{id}` | Delete virtual-models id | — |
| `POST` | `/admin/virtual-models/{id}/defaults` | Create virtual-models id defaults | `model_id`, `weight`*, `position` |
| `POST` | `/admin/virtual-models/{id}/rules` | Create virtual-models id rules | `position`, `match_condition` |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Traps

**A model and a virtual model cannot share a name.** The API returns 409:
a client request naming it would be ambiguous. Enforced in `post_virtual_model`
against `model_name_exists`, and pinned by
`a_model_and_a_virtual_model_cannot_share_a_name`. Writing straight to Postgres
bypasses that check and creates exactly the ambiguity the constraint exists to
prevent.

**`virtual_model_defaults.position` is `NOT NULL` with no default.** An INSERT
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
curl -sk -b /tmp/ck https://192.168.10.129:4001/admin/virtual-models

# what a request would actually do
curl -sk -b /tmp/ck -X POST https://192.168.10.129:4001/admin/routing/dry-run \
  -H 'content-type: application/json' -d '{"model":"<virtual>","prompt_tokens":100}'
```

Changes reach the proxies on their snapshot poll, not instantly. A gateway
`401` means the proxy is healthy and rejecting an unauthenticated request; a
`/health` `503` means it has no usable snapshot yet.
