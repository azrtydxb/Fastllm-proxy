# Control plane, RBAC, and rule-based routing

Design for turning fastllm-proxy from a config-file gateway into a managed one:
real API keys with role-based authorisation, virtual models with routing rules,
rate limits, usage accounting, and a management UI.

Status: design agreed 2026-08-06, not yet implemented.

## What already exists

Worth stating, because one of the four asks is already done and two of the
others build directly on what is there.

- **Multiple models already work.** `model_list` takes any number of
  `model_name` entries, several entries sharing a name form a load-balanced
  pool, and an entry whose `litellm_params.model` differs is an alias whose
  request body is rewritten on the way through. `/v1/models` aggregates across
  all of them. Nothing in this design changes that; it moves where the list is
  *stored*.
- **Routing already exists at the pool level** — cache-affinity, least-loaded
  and round-robin, with health tracking and in-flight accounting. What is
  missing is a layer *above* it that decides which pool a request belongs to.
- **The proxy has no state and no dependencies.** Config file in, atomically
  swapped registry, and a request path that does no I/O. Measured against the
  live spark2 replica, the proxy's own overhead is zero: TTFT 83.2ms through a
  local proxy against 83.8ms direct, inter-token 27.51ms against 27.46ms.

That last property is the thing this design most has to avoid destroying.

## The constraint that shapes everything

**Authorisation must not put I/O on the request path.**

A Postgres round trip is 1–5ms. The entire proxy overhead today is under
0.1ms. Looking a key up in a database per request would cost more than
proxying the request — an order of magnitude more than the connection-layer
rewrite that took streaming throughput up 6x.

So the forwarding path never touches the database. It reads an in-memory
snapshot, and every expensive question — which roles does this key have, what
do those roles grant, does that wildcard match — is answered once when the
snapshot is loaded, not per request.

## Architecture

### One binary, three roles

The split between management and forwarding is a **runtime role**, not a
deployment boundary. Same image everywhere; `--role` decides what runs.

| role | runs | for |
|---|---|---|
| `all` | control plane and forwarding in one process | lab, laptop, single container |
| `control` | database, admin API, UI, `/snapshot` | production management |
| `proxy` | forwarding only | production data plane, scaled to N |

Two binaries would have made the single-container case a second-class citizen
and created a code path only reachable in Kubernetes. A role flag makes `all`
the default and the split an operational choice taken on the way to
production.

**Lab:** `docker compose up` — stock `postgres:17` plus our image with
`--role=all`.

**Production:** CloudNativePG plus our image twice — `--role=control` (one or
two replicas) and `--role=proxy` (N, stateless, horizontally scalable).

Our code is always exactly one container image.

### The snapshot is the only contract

The control plane builds a single versioned document from the database. The
data plane consumes it and nothing else. The proxy never queries the database,
never writes to it, and links no database driver.

```
Snapshot {
  version:      u64,        // monotonic, assigned by the control plane
  generated_at: timestamp,
  keys:         [ { hash, principal_id, expires_at } ],
  principals:   [ { id, allowed_models, limits, budget } ],
  models:       [ { name, backends[] } ],          // today's model_list
  virtual:      [ { name, rules[], targets[] } ],  // P1
}
```

`allowed_models` is **pre-flattened**: roles resolved, wildcards expanded, deny
rules applied. The request path asks a `HashSet`, never walks the RBAC graph.
This is the single most important line in the design.

### Three snapshot sources

One trait, three implementations. Forwarding cannot tell them apart.

| source | used by | depends on |
|---|---|---|
| `File` | `--role=proxy` with no control plane | nothing |
| `Local` | `--role=all` | Postgres, in-process |
| `Http` | `--role=proxy` against a control plane | control plane, with disk cache |

`File` mode is kept deliberately. It is today's behaviour, it has no
dependencies, and it means the proxy we have measured and deployed does not
evaporate the moment RBAC exists. A laptop pointed at spark2 with a YAML file
must keep working.

### Where configuration lives

Dual sources of truth would be misery, so the split is by kind, not by
convenience:

- **Process config** stays in the file: bind address, role, database URL, poll
  interval, TLS, proxy token. None of it is meaningfully hot-reloadable.
- **Policy** moves to Postgres: models, keys, roles, permissions, virtual
  models, routing rules, limits, budgets.

**This supersedes the ConfigMap live-reload** for anything policy-shaped. That
mechanism keeps earning its place in `File` mode, but under `control`/`proxy`
the snapshot replaces it. `fastllm-proxy import --config litellm_config.yaml`
seeds the database from an existing file in one command, which also preserves
the LiteLLM-compatibility story.

### Snapshot protocol

- `GET /snapshot` with `If-None-Match`; `304` when unchanged, full document
  otherwise. Poll interval ~1s, so a revoked key stops working in about a
  second.
- The proxy writes each accepted snapshot to disk as last-known-good.
- The proxy authenticates with its own bootstrap token from env or a Secret,
  distinct from any user key. `/snapshot` is read-only, so a stolen proxy token
  discloses policy — including key *hashes*, never plaintext — and grants
  nothing else.
- `POST /usage` is the reverse channel: batched, fire-and-forget, dropped on
  failure. Defined in P0 even though nothing sends to it until P2, so the
  protocol does not need reshaping later. Dropping usage rather than blocking a
  request is deliberate — billing accuracy is not worth failing inference.

## Data model

Full RBAC, covering two axes that are easy to conflate and must not be:

- **Admin authz** — who may create keys, edit routing rules, read usage. Applies
  to the control plane's API and UI.
- **Inference authz** — which models a key may invoke. Applies to the request
  path.

Both fall out of one `roles → permissions (verb, resource)` model. Only the
second is on the hot path, and only the second is flattened into the snapshot.

A **principal** is the thing permissions attach to: either a human `user` who
signs into the UI, or a `service_account` that owns API keys. Roles attach to
principals, never to individual keys — a key inherits its owner's grants, so
rotating a key never silently changes what it can reach.

```
principals       id, kind, name, email, password_hash, disabled, created_at
                 -- kind: 'user' | 'service_account'; password_hash only for 'user'
roles            id, name, description
permissions      id, verb, resource        -- model:invoke, key:create, config:write
role_permissions role_id, permission_id
principal_roles  principal_id, role_id

api_keys         id, hash, prefix, name, principal_id, expires_at,
                 disabled, created_at, last_used_at

models           id, name, description
model_backends   id, model_id, api_base, upstream_model, upstream_api_key
                 -- encrypted at rest with a key from the environment, and
                 -- never returned by the admin API. It IS carried usably in
                 -- the snapshot, because the proxy must present it to the
                 -- backend -- so /snapshot must be TLS in any deployment
                 -- where backends have real credentials. Unlike user API
                 -- keys, this one cannot be reduced to a hash.
virtual_models   id, name, description
routing_rules    id, virtual_model_id, position, match_json
rule_targets     id, rule_id, model_id, weight, position

limits           principal_id, requests_per_min, tokens_per_min
budgets          principal_id, tokens_total, tokens_used, window_start, window
usage_events     id, principal_id, model_id, prompt_tokens, completion_tokens, at
                 -- append-only audit log; budgets.tokens_used is the running
                 -- counter the snapshot carries, reconciled from these
```

Migrations with `sqlx`. Postgres only — chosen deliberately over supporting two
backends, because SQL dialect differences leak into the query layer and double
the test surface for a system whose write rate is a few rows a day.

### Two hashing decisions, deliberately different

- **API keys use SHA-256.** They are high-entropy random values, so a slow KDF
  buys nothing and would cost a great deal — Argon2 per request would dwarf the
  entire proxy. Plaintext is displayed once at creation and never stored.
- **User passwords use Argon2id.** They are low-entropy and human-chosen, which
  is exactly the case a slow KDF exists for, and login is not a hot path.

Getting these the wrong way round is a classic error in both directions.

## The request path

Per request, after the snapshot is in memory:

1. Extract the bearer token (scheme already case-insensitive per RFC 7235).
2. SHA-256 it (~200ns) and look up the principal. Miss → 401.
3. Compare `expires_at`. Expired → 401.
4. Resolve the requested model name; if virtual, evaluate rules (P1) to a
   concrete model.
5. One `HashSet` lookup: may this principal invoke that model? No → 403.
6. Rate limit check: one lookup and an atomic decrement (P2). Over → 429 with
   `Retry-After`.
7. Route within the pool using the existing policy, and forward.

Sub-microsecond, no I/O, no allocation beyond what exists today. Against
spark2's 27ms per token this is unmeasurable, which is the point.

A test should assert the request path performs no I/O, because that is the
property most likely to rot.

## P1 — Virtual models and routing rules

A virtual model is a client-facing name with an ordered list of rules. The
first rule whose conditions match supplies the targets; if none match, the
virtual model's defaults apply.

**Match conditions**, all four requested:

- **Caller** — principal, role. Free: identity is already resolved by step 2.
- **Request shape** — approximate prompt size and requested `max_tokens`. Real
  tokenisation on the request path is out of the question, so size is estimated
  from body bytes (~3.5 bytes per token for English) and `max_tokens` is read
  from the body, which is already being parsed for `model`. This is an
  *estimate* and the docs must say so rather than implying precision.
- **Health and load** — targets are an ordered chain; an unhealthy or saturated
  target falls through to the next. Mostly exposing machinery that already
  exists.
- **Weighted split** — percentages across targets, for canary and A/B.

**Weighted split versus prefix affinity.** These genuinely conflict: a hashed
prefix wants to stick to one node, a percentage split wants to spread. The
resolution is that the split decides *which model*, and affinity then decides
*which replica within that model's pool*. Further, the weighted choice is made
by hashing the same request prefix rather than by RNG, so a given conversation
lands on the same side of a canary every time. Deterministic split preserves
cache locality; random split would destroy it on every turn.

**No recursion.** A virtual model targets concrete models only. Virtual models
targeting virtual models invites cycles and an evaluation-depth limit for no
real benefit.

## P2 — Rate limiting

Limits attach to a principal: requests per minute and tokens per minute.

Enforcement is a local token bucket per principal per replica — one lookup and
an atomic decrement, no I/O. Accuracy comes from **periodic reconciliation**:
every few seconds each proxy reports its counts to the control plane, which
returns each replica's allowance for the next window based on observed share.

This converges within seconds without putting Redis on the request path. The
honest cost: a limit can be exceeded by up to one reconciliation window's worth
of traffic during a sharp spike. Accepted deliberately — the alternative is a
network round trip per request, which is precisely what this proxy exists to
avoid.

In `--role=all` the machinery is inert: one process means local counters are
already global and exact.

## P3 — Usage accounting and budgets

The awkward one, because usage lives in the response body and the proxy's
defining behaviour is that it never parses response bodies.

**Getting the numbers.** Non-streaming responses carry a `usage` object.
Streaming responses carry one only when `stream_options.include_usage` is set,
so the proxy injects that field for principals with token limits or budgets —
using the body-rewriting path that already exists for aliases.

**Reading them without becoming a parser.** The proxy keeps a small ring buffer
of the last few KB forwarded and parses it once, at end of stream. Usage always
appears in the final event, so a tail buffer finds it. Every byte still reaches
the client untouched and no frame is inspected in flight. This costs one small
parse per request, not per frame.

**Two consequences to be honest about:**

1. Injecting `include_usage` **adds a usage chunk the client did not ask for**.
   It is therefore enabled per principal, only when limits or budgets are
   configured, never globally.
2. Budgets are enforced *after the fact*. Consumption is known only once a
   response completes, so a request that blows the budget still completes; the
   next one is refused. Mid-request cancellation would mean counting tokens as
   they stream, which means parsing every frame.

## P4 — Management UI

Served by `--role=control` and `--role=all` only; the proxy role serves no UI.
Embedded in the binary with `rust-embed`, per the existing `TODO.md` entry, so
the single-container story holds and there is no second artefact to deploy.

Views: models and backends with live health, virtual models and their rule
chains, keys with creation and revocation, roles and permissions, usage and
budgets, and backend health from the data the proxy already exposes.

Admin authentication is a session cookie backed by Argon2id passwords, distinct
from the API keys used for inference. The build wires a `node` stage into the
existing multi-stage Dockerfile rather than a `build.rs` calling npm, so
`cargo test` stays node-free.

## Failure modes

| event | behaviour |
|---|---|
| control plane down, proxy warm | serves from memory indefinitely; warns; policy stops changing |
| control plane down, proxy cold | loads last-known-good from disk and serves |
| cold start, no cache at all | starts, `/health` unhealthy, inference 503s with a clear reason — never crash-loops |
| snapshot invalid | keeps the previous one, logs once (the rule the config watcher already follows) |
| key revoked | effective within the poll interval, ~1s |
| Postgres down | control plane serves its last built snapshot read-only; proxies unaffected |
| usage report fails | dropped; never blocks or fails a request |

Never crash-looping on a cold start with no cache is a deliberate choice: under
Kubernetes a crash-loop turns a control-plane outage into a data-plane outage,
which is the failure this architecture exists to prevent.

## Testing

`--role=all` makes integration testing tractable: the whole system runs in one
process, so control plane and data plane can be tested together without a
cluster or a compose file.

- **Unit** — authz flattening (roles, wildcards, deny precedence), key hashing
  and expiry, rule matching and target selection, weighted split determinism,
  snapshot merge and version ordering.
- **Integration** — `--role=all` against the existing mock upstream: key
  lifecycle end to end, revocation latency, 401/403/429 paths, virtual model
  routing including failover, budget enforcement across a reconciliation window.
- **Property** — the request path performs no I/O.
- **Migration** — `import --config` reproduces the behaviour of the same file
  in `File` mode.

The mock upstream, load generator and latency harness used for the performance
work currently live in a session scratchpad and are **not in the repo**. They
should move into `bench/` as part of P0, since P1 and P2 both need them.

## Build order

| | delivers | depends on |
|---|---|---|
| **P0** | control plane, snapshot contract, RBAC, real keys, per-model authz, expiry | — |
| **P1** | virtual models and routing rules | P0 |
| **P2** | rate limits with reconciliation | P0 |
| **P3** | usage accounting and budgets | P0, P2's channel |
| **P4** | management UI | P0 |

P0 is load-bearing; nothing else is reachable without the snapshot and
identity. P1 is the feature with the most user-visible value and does not
depend on P2 or P3, so it should come second even though P2 is easier.

## Risks

- **The hot path is the thing to protect.** Every feature here adds a
  temptation to do one more lookup per request. The pre-flattened snapshot and
  the no-I/O test are the guardrails.
- **This repo now owns connection-pool correctness**, and P0 adds a control
  plane, a database and a sync protocol on top. The four bugs found in the
  connection rewrite — all caught by the mock, none by reading the code — are
  the argument for building the harness into the repo before, not after.
- **Postgres becomes a hard dependency of the control plane.** The data plane
  is insulated by design, but the admin surface is not, and `--role=all` makes
  the whole thing unavailable if the database is.
- **P3 pushes against the byte-pump design.** The tail-buffer approach keeps it
  to one parse per request, but it is the feature most likely to erode the
  property that makes this proxy worth having, and should be the first thing
  reconsidered if it grows.
