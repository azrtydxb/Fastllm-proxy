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
change here — it is how you check a rule does what you meant.

**It cannot evaluate `max_inflight_per_backend`.** The dry-run runs on the
control plane, whose registry has no in-flight counters and no engine scrape,
so every backend looks idle and a spill rule always reports as still matching.
Only real traffic exercises that condition.

<!-- BEGIN GENERATED: endpoints -->

| Method | Path | Summary | Body fields |
|---|---|---|---|
| `DELETE` | `/admin/frontend-model-defaults/{id}` | Delete frontend-model-defaults id | — |
| `GET` | `/admin/frontend-models` | Read frontend-models | — |
| `POST` | `/admin/frontend-models` | Create frontend-models | `name`, `description`* |
| `PATCH` | `/admin/frontend-models/{id}` | Change how a frontend model chooses between its targets | `name`*, `description`* |
| `DELETE` | `/admin/frontend-models/{id}` | Delete frontend-models id | — |
| `POST` | `/admin/frontend-models/{id}/defaults` | Create frontend-models id defaults | `provider_model_id`*, `model_pool_id`*, `weight`*, `position` |
| `POST` | `/admin/frontend-models/{id}/rules` | Add a routing rule. First match wins, and the matching rule decides everything — every action is terminal | `position`, `action`*, `deny_status`*, `deny_message`*, `jump_to`*, `tag`*, `match_condition` |
| `DELETE` | `/admin/model-pool-members/{id}` | Remove one model from its pool | — |
| `GET` | `/admin/model-pools` | Named groups of provider models, each with one policy for choosing between them | — |
| `POST` | `/admin/model-pools` | Create a pool. Its name must not collide with a provider model or a frontend model, because routing resolves targets by name | `name`, `description`*, `policy`* |
| `PATCH` | `/admin/model-pools/{id}` | Rename a pool, change its description, or change how it chooses between its members. A rename carries onto the targets pointing at it | `name`*, `description`*, `policy`* |
| `DELETE` | `/admin/model-pools/{id}` | Delete a pool. Refused while any rule target or default still routes to it, rather than cascading into rules that point at a name which no longer resolves | — |
| `POST` | `/admin/model-pools/{id}/members` | Add a provider model to a pool. The same model twice is refused: a member counted twice is one machine counted twice | `provider_model_id`, `weight`*, `position`* |
| `POST` | `/admin/routing/dry-run` | Which rule would decide, and what the chain resolves to, without dispatching | `model`, `principal_id`*, `streaming`*, `prompt_tokens`*, `max_tokens`*, `headers`*, `class`*, `class_refines`* |
| `DELETE` | `/admin/rule-targets/{id}` | Delete rule-targets id | — |
| `PATCH` | `/admin/rules/{id}` | Change how a rule chooses among its targets, or where it sits in the order. Its conditions are not editable: delete and recreate instead of letting a rule change meaning while keeping the position that makes it first | `position`*, `match_condition`*, `action`*, `deny_status`*, `deny_message`*, `jump_to`*, `tag`* |
| `DELETE` | `/admin/rules/{id}` | Delete rules id | — |
| `POST` | `/admin/rules/{id}/targets` | Create rules id targets | `provider_model_id`*, `model_pool_id`*, `weight`*, `position` |

*\* optional field*

<!-- END GENERATED: endpoints -->

## Actions

Every rule has one, and every one is terminal — no rule contributes and passes
on, so `dry-run` names a single deciding rule.

- `route` (default) — the rule's targets, ordered by its `policy`.
- `deny` — refuse. Needs `deny_status`, **4xx only** (a 5xx would have every
  client library retrying something that is never going to be allowed).
- `jump` — continue in another frontend model's chain (`jump_to`), so shared
  policy is written once. A loop is refused at write time; a destination that
  was deleted is treated as no match and falls through.

`tag` is a field, not an action: it labels the usage rows the rule produced,
so spend is answerable per decision. A rule that tags still routes.

A `deny` shows up in `dry-run` as a `denied` object with the status and
message, and an empty `candidates` — which is a different thing from "nothing
is serving", and the distinction is the point.

**Cost as a condition** (`min/max_request_cost_micros`) is priced at the
cheapest model the frontend model can reach — one number per request, the same
for every rule, so it does not depend on which rule is asking. Pairs with
`deny` ("refuse anything over $0.50 however I route it"). Sending the expensive
ones somewhere cheaper is not this; order the chain with the cheaper target
first. All
targets unpriced means no cost, and the rule declines.

## Three levels, and they answer different questions

**A rule's targets are an ordered failover chain.** No policy, no weighted
split — tried in the order written. `POST /admin/rules/{id}/targets` takes
either `provider_model_id` **or** `model_pool_id`, never both.

**A pool** (`/admin/model-pools`) is a named group of provider models with one
policy: `cache-affinity`, `least-loaded`, `lowest-latency`, `round-robin`, or unset
for the weighted split by member weight. Cost is not one of them — a price is
fixed, so it never balances; use a rule's `min/max_request_cost_micros`. Two pools may
hold the same members and differ only in policy — that is what naming it buys.
A pool expands in place: its chosen member, then its others, then the chain's
next target.

**A provider model's `policy`** chooses between its own attachments (the
providers serving that one model). Set with `PATCH
/admin/provider-models/{id}`.

`routing_rules.policy` and `frontend_models.policy` no longer exist; a rule
that balanced across its targets is a rule pointing at a pool.

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

**It counts the engine's requests, not this proxy's.** Each proxy scrapes every
backend's Prometheus `/metrics` every two seconds
(`--engine-scrape-interval`, `0` disables) and compares the ceiling against
vLLM/SGLang's own running + queued count, so a limit of 2 still means 2 with
three proxies running. A backend with no `/metrics`, or one whose last reading
is over ten seconds old, falls back to this replica's own count.

**Which backends have metrics is detected, not configured.** A backend that
produces no reading is retried every five minutes and given up on after
fifteen, at which point it is dropped from the scrape; one that *answers* with
something that is not engine metrics settles on the second answer. Nothing
needs to mark a provider as an engine, and nothing polls a cloud vendor in a
loop. A backend given up on by deadline — nothing ever replied — gets one more
look if it later passes a health probe, so an engine that was slow to load is
not written off permanently.

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
