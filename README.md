# fastllm-proxy

A low-latency OpenAI-compatible gateway for multi-node LLM serving, written in Rust.

It fronts several inference backends (vLLM, SGLang, llama.cpp — anything speaking the OpenAI API) behind one endpoint, and it is built for the case where a general-purpose gateway costs you throughput instead of adding it.

It reads LiteLLM-format config files unchanged, so it drops into an existing `sparkrun proxy` setup without rewriting anything.

## Architecture

Component and request-flow diagrams, and the consistency guarantees stated
plainly, are in [docs/architecture.md](docs/architecture.md).

## Why

Two separate things go wrong when a conventional gateway sits in front of prefix-caching inference engines.

**Round-robin destroys the prefix cache.** vLLM and SGLang keep a radix/prefix KV cache. Two requests sharing a system prompt are far cheaper on the *same* node — the second reuses the first's cached prefix instead of prefilling it again. A round-robin balancer alternates them by construction, so every request pays full prefill. Nodes look evenly loaded while aggregate throughput drops, and adding a second node can make things *worse* than one.

**Per-token work in the proxy is per-token overhead.** A gateway that deserialises each SSE chunk to re-emit it is doing thousands of parse/re-encode cycles per second per stream. That is latency added to every token.

fastllm-proxy addresses both: routing is prefix-aware, and response bodies are never parsed.

## Design

**Responses are opaque bytes.** Upstream frames are handed to the client exactly as they arrive — never deserialised, never re-encoded, never buffered. An SSE stream is a pointer move per frame.

**Requests are inspected once.** The body is parsed only to read `model`, and the *original* bytes are forwarded. The body is rebuilt only when an alias means the upstream expects a different model name.

**Cache-affinity routing with a load escape hatch** (default). Hash a prefix of the request, send it to whichever backend served that prefix last — unless that backend is meaningfully more loaded than the least-loaded one, in which case take the cache miss and rebalance. A load-driven deviation deliberately does *not* re-claim the prefix: its KV cache still lives on the original node, so a momentary spike must not migrate a hot prefix permanently.

**In-flight is counted for the whole generation.** The counter is released when the last token is streamed, not when upstream headers arrive. Getting this wrong makes every streaming backend look idle and silently collapses least-loaded routing into round-robin.

**Config reloads in place.** `SIGHUP` rebuilds the routing table and swaps it atomically; in-flight generations are unaffected and health state carries over. Adding or removing a model does not mean restarting a gateway with work in flight.

## Performance

Every number below was measured, on the hardware named, on the date named.
Nothing here is projected or scaled from a smaller test. Where two runs of the
same thing disagree, both are shown.

### Against a real vLLM, the proxy is not measurable

Two runs against the live spark2 replica (arm64, `qwen3-6-35b-a3b-nvfp4`),
2026-08-06. "Direct" is the same client against the same vLLM with no proxy in
the path:

| | through the proxy | direct | delta |
|---|---|---|---|
| TTFT, run 1 | **83.2 ms** | 83.8 ms | −0.6 ms |
| TTFT, run 2 | **90 ms** | 93 ms | −3 ms |
| inter-token | **27.51 ms** | 27.46 ms | +0.05 ms |
| 8 concurrent streams, aggregate | **121.8 tok/s** | 118.7 tok/s | +3.1 tok/s |
| 8 concurrent, inter-token | 63.4 ms | 62.2 ms | +1.2 ms |

The proxy comes out marginally *ahead* on two of these, which is not a claim
that a proxy makes inference faster — it is run-to-run noise on a live GPU, and
that is the point: the overhead is below the noise floor of the thing it sits
in front of.

### Where the time actually goes

The proxy's own per-request work totals **~0.76 µs**, against ~38 µs of core
time per request. Roughly 2%; the rest is kernel, socket and HTTP protocol work
that any process doing this job would pay.

| step | cost |
|---|---|
| URL format + `Uri` parse | 156 ns |
| `BodyPeek` parse (1 KiB body) | 229 ns |
| prefix hash for affinity | 108 ns |
| header copy | 97 ns |
| bearer header build | 92 ns |
| path allocations | 41 ns |

Authorisation, rate limiting and budget checks are not in that table because
they are set lookups against a pre-flattened in-memory snapshot — no database
call, no network call, no file read on the request path. `tests/no_io_on_hot_path.rs`
fails the build if that changes.

### Synthetic ceilings

10-core arm64 macOS, `--release`, driven by a mock SSE upstream that flushes
every frame with no think time. That is the **worst case** for framing
overhead — a real vLLM emits one SSE event per HTTP frame tens of milliseconds
apart, so production numbers are better than these.

| | measured |
|---|---|
| streaming | **7,921 req/s**, 471 MiB/s |
| non-streaming, 64 KiB bodies | **67,200 req/s** |
| frame ceiling (any frame size) | ~650,000 frames/s |
| raw byte pump | ~3 GB/s |

### What one architectural decision was worth

The upstream client used to be a pooled `hyper_util` client, which cost one
cross-task wakeup per frame. Replacing it with a connection this process owns
and drives from inside the response body ([src/upstream.rs](src/upstream.rs)):

| | before | after |
|---|---|---|
| streaming | 1,314 req/s | **7,921 req/s** |
| throughput | 78 MiB/s | **471 MiB/s** |
| non-streaming | unchanged | unchanged |

A little over **6x**, and it is why response bodies are never parsed: the win
came from deleting a wakeup, not from batching. Coalescing already-arrived
frames was measured at a merge ratio of exactly 1.000 in three separate
settings — there is never a second frame waiting.

### How much is left

A dumb bidirectional TCP relay — parsing nothing, framing nothing, the hard
floor for any proxy — was measured on the same path:

| | throughput |
|---|---|
| direct hop, no proxy | 951 MiB/s |
| dumb TCP relay | 694 MiB/s |
| **fastllm-proxy** | **524 MiB/s** |

So the entire remaining prize over a proxy that understands nothing is
**1.32x**, and only if detecting end-of-response were free — it is not, since
the in-flight guard has to be released and the connection reused.
[TODO.md](TODO.md) records what was tried and rejected, with numbers, so nobody
re-litigates it from intuition.

### Classifier tiers

Semantic routing costs what it measures. Same machine, `--release`,
`bench/potion` and `bench/minilm`:

| tier | model | p50 per prompt | what it separates |
|---|---|---|---|
| 1 | potion-base-8M | 103 µs | subject-matter classes |
| 1 | **potion-code-16M** | **115 µs** | best on coding (98.7%) |
| 1 | potion-retrieval-32M | 137 µs | best all-round (90.0%) |
| 2 | all-MiniLM-L6-v2 | 1.66 ms | modest gain over tier 1 |
| 2 | **bge-small-en-v1.5** | **3.27 ms** | same-subject/different-intent |

Tier 1 is a token-vector lookup and a mean — no transformer, no matmul. Cost
also *plateaus* rather than growing with the prompt, because the encoder stops
at its token cap: a 64 KB paste costs exactly what a 4 KB one does.

Measured accuracy, held out over ~21k human-labelled prompts
(`HuggingFaceH4/no_robots`, `openai/gsm8k`, eleven StackExchange communities):

| class | tier 1 precision | recall |
|---|---|---|
| coding | 97.6% | 92.6% |
| chat | 95.8% | 98.0% |
| generation | 96.8% | 69.6% |
| math | 88.0% | 97.6% |
| devops | 86.8% | 90.5% |
| finance | 86.2% | 91.6% |
| legal | 85.9% | 75.0% |
| security | 84.8% | 82.3% |
| factual-qa | 83.7% | 50.4% |
| databases | 82.3% | 91.1% |

Tier 2 is consulted **only** when a routing rule names a class that needs it.
If no rule does, the transformer is never loaded and no request can pay for it.
On realistic traffic mixes escalation touches under a tenth of requests, which
puts the average added cost near 0.2 ms.

Two findings worth knowing before configuring classes:

- **Classify by subject, not by verb.** Subject-matter classes (legal, finance,
  security, coding) reach 82-98% precision on tier 1. Task-shaped classes
  (summarise, rewrite, extract) fail on *both* tiers — under bge-small,
  Summarize scores 46.6% and Extract 35.6%, worse than tier 1. Telling
  "summarise this" from "extract the dates" needs instruction understanding,
  not better embeddings.
- **Margins are not comparable across models.** bge-small reports higher raw
  cosine similarities than the static model while classifying better, because
  its space is anisotropic. Confidence floors are calibrated per class *and*
  per tier.

### What has not been measured

There is no benchmark here against another gateway. The comparisons above are
against a direct connection and against a dumb TCP relay — the floor and the
ceiling — not against LiteLLM, Envoy or anything else. Claims of the form
"faster than X" are not made because that measurement has not been run.

Reproduce any of this with `cargo run -p bench --release --bin realbench`
(and siblings) — see [bench/](bench/).

## Install

```bash
cargo build --release
# target/release/fastllm-proxy
```

## Roles

One binary, three ways to run it, via `--role` (`FASTLLM_ROLE`):

| Role | What it does | Needs |
|---|---|---|
| `proxy` (default) | Forwarding only, against either a control plane (`Http` mode) or a config file (`File` mode) | `--control-url` + `--proxy-token` (`Http` mode), or `--config` alone (`File` mode) |
| `all` | Control plane and forwarding in one process, sharing state directly — no HTTP round trip between them | `--database-url`, `FASTLLM_ENCRYPTION_KEY` |
| `control` | Database, admin API (`/admin/*` — keys, principals, roles, models, backends), `/snapshot` and `/usage` — no proxy listener | `--database-url`, `FASTLLM_ENCRYPTION_KEY` |

`proxy` is the default deliberately, not `all`: it is the only role that asks for nothing beyond what a pre-control-plane deployment already passed (`--config` and nothing else), so an existing deployment upgrades to this binary without gaining a new required flag. `all` and `control` are explicit opt-ins via `--role`/`FASTLLM_ROLE`.

`control` and `proxy` split for a cluster deployment where the admin API and management UI need to stay off the public listener while the proxy scales out independently — network isolation is still the right default even now that `/admin/*` has its own session-cookie authentication (see below), the same way keeping a login-gated internal tool off a public LoadBalancer is good practice regardless of the login. See `deploy/` for the manifests that wire this up on Kubernetes.

`Http` mode degrades gracefully: a `proxy` that cannot reach its control plane at startup, or loses it later, falls back to the last snapshot it wrote to `--snapshot-cache` (default `/var/lib/fastllm/snapshot.json`) rather than refusing to start or dropping traffic.

### Migrating a `File`-mode deployment onto a database

```bash
fastllm-proxy import --config litellm_config.yaml --database-url postgres://...
```

Idempotent — seeds `models`/`model_backends` **and the `auth:` block** (a `service_account` principal per key, the key itself as a SHA-256 hash, and its model grants) from a LiteLLM-format config, and can be run more than once safely. Point `--role=all`/`control` at the same database afterward and the same keys keep working, with the same per-model authorisation they had in `File` mode.

Each imported key gets its own role, `import:<name>`, holding just that key's grants — `models: ['*']` becomes `model:invoke` on `model/*` (i.e. allow-all), a named list becomes one grant per model. Re-importing an edited file converges: grants dropped from the file are revoked, not merely left behind. `import` never prints a key back; the config file is the only copy of the plaintext.

Day-to-day changes after the initial seed go through the admin API below rather than another `import` run or hand-written SQL, so they reach a running control plane immediately instead of on its next periodic rebuild.

### Quickstart with docker-compose

`docker-compose.yml` in the repo root brings up Postgres and `--role=all` together:

```bash
docker compose up
# proxy on :4000, admin API on :4001, postgres on :5432
FASTLLM_ENCRYPTION_KEY=0000000000000000000000000000000000000000000000000000000000000000 \
  fastllm-proxy import --config litellm_config.yaml \
  --database-url postgres://fastllm:fastllm@localhost:5432/fastllm
curl -XPOST localhost:4001/admin/keys -H 'content-type: application/json' \
  -d '{"name":"local","principal_id":1}'
```

Principal `1` is the `bootstrap` service account the migrations seed, already
holding the `inference` role. For anything beyond a first key, create your own:

```bash
# A principal, then a role for it, then a key against it.
curl -XPOST localhost:4001/admin/principals -H 'content-type: application/json' \
  -d '{"name":"ci-pipeline"}'                       # -> {"id":2,...}
curl -XPOST localhost:4001/admin/principals/2/roles -H 'content-type: application/json' \
  -d '{"role":"inference"}'
curl -XPOST localhost:4001/admin/keys -H 'content-type: application/json' \
  -d '{"name":"ci","principal_id":2,"expires_at":"2027-01-01T00:00:00Z"}'
```

(`import`, run here on the host rather than inside the container, needs the
same `FASTLLM_ENCRYPTION_KEY` `docker-compose.yml` sets for `--role all` —
they share one database, so they must agree on one key.)

## Usage

```bash
fastllm-proxy --config litellm_config.yaml --host 127.0.0.1 --port 4000
```

Point clients at it as an OpenAI endpoint:

```bash
curl http://localhost:4000/v1/chat/completions \
  -H 'Authorization: Bearer sk-...' \
  -H 'content-type: application/json' \
  -d '{"model":"Qwen/Qwen3-1.7B","stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

Reload after the model set changes — no restart, no dropped streams:

```bash
kill -HUP $(pgrep -x fastllm-proxy)
```

### Endpoints

| Endpoint | Purpose |
|---|---|
| `POST /v1/chat/completions` | Proxied. Also `/completions`, `/embeddings`, `/rerank`, `/score`, `/audio/*` |
| `GET /v1/models` | Aggregated across every pool |
| `GET /health` | Per-backend health, in-flight, request and error counts. No auth required. Exposes backend addresses — keep it off the public interface |
| `GET /metrics` | Prometheus text. No auth required |
| `/admin/*` | `--role all`/`control` only. Gated by a session cookie (`POST /login`), not `--proxy-token` — see the table below and "Admin authentication" underneath it |
| `POST /login` / `POST /logout` | `--role all`/`control` only. Argon2id password check; sets/clears the `fastllm_session` cookie every other `/admin/*` route requires |
| `/`, `/ui/*` (management UI) | `--role all`/`control` only. The embedded SPA — see "Management UI" below |
| `GET /snapshot` | `--role all`/`control` only. What `--role proxy` polls in `Http` mode; gated by `--proxy-token` |
| `POST /usage` | `--role all`/`control` only. Batched usage reporting from `--role proxy` (see "TLS and the reverse channel" below); gated by the same `--proxy-token` as `/snapshot` |
| `POST /limits/reconcile` | `--role all`/`control` only. Rate-limit count reporting from `--role proxy` (see "Rate limits" below); gated by the same `--proxy-token` |

#### Admin API

Everything an operator needs to run the control plane, so that neither raw SQL nor a second `import` run is the documented way to change policy. Every mutating route rebuilds and republishes the snapshot on the spot, so a change reaches `--role proxy` within one `--config-poll` interval rather than waiting on the control plane's own periodic rebuild.

| Endpoint | Purpose |
|---|---|
| `GET /admin/principals` | Principals with their roles |
| `POST /admin/principals` | `{"name":..., "kind":..., "email":...}`. `kind` is `service_account` (the default) or `user` |
| `DELETE /admin/principals/{id}` | Cascades to that principal's keys and role grants |
| `POST /admin/principals/{id}/roles` | `{"role":"inference"}`. Idempotent |
| `DELETE /admin/principals/{id}/roles/{role}` | Revoke one role |
| `PUT /admin/principals/{id}/password` | `{"password":...}`. Argon2id-hashes it and promotes the principal to `kind = 'user'` if it was not already |
| `GET /admin/keys` | Prefix, name, principal, expiry, disabled. **Never** the key or its hash |
| `POST /admin/keys` | `{"name":..., "principal_id":..., "expires_at":...}`. Returns the plaintext key once |
| `DELETE /admin/keys/{id}` | Revoke (sets `disabled`; the row stays for audit) |
| `GET /admin/models` | Models and their backends. Reports *whether* a backend has an upstream credential, never the credential |
| `POST /admin/models` | `{"name":..., "description":...}` |
| `DELETE /admin/models/{id}` | Cascades to that model's backends |
| `POST /admin/models/{id}/backends` | `{"api_base":..., "upstream_model":..., "upstream_api_key":..., "protocol":..., "auth_header":..., "auth_scheme":..., "default_max_tokens":...}`. Everything after the credential is optional and defaults to an OpenAI-compatible upstream reached with `Authorization: Bearer`. The credential is encrypted before it reaches Postgres and cannot be read back |
| `DELETE /admin/backends/{id}` | Remove one backend from a pool |
| `GET /admin/roles` | Roles and the permissions each one grants |
| `GET /admin/limits` | Every principal with a configured rate limit |
| `PUT /admin/principals/{id}/limits` | `{"requests_per_min":..., "tokens_per_min":...}`. Either or both; upserts the one row this principal may have |
| `DELETE /admin/principals/{id}/limits` | Remove the limit — the principal becomes unlimited, not limited to zero |
| `GET /admin/budgets` | Every principal with a configured token budget, including current consumption |
| `PUT /admin/principals/{id}/budget` | `{"tokens_total":..., "window":"daily"\|"weekly"\|"monthly"}`. Upserts the one row this principal may have; leaves `tokens_used` and the window's start alone on an update |
| `DELETE /admin/principals/{id}/budget` | Remove the budget — the principal becomes unlimited, not limited to zero |

**No route returns a credential.** Key plaintext is shown once, by `POST /admin/keys`, and never again; `api_keys.hash` is a verifier, not a display value, and is not in any response. `upstream_api_key` is the one secret that cannot be reduced to a hash — the proxy has to present it upstream — so it is encrypted at rest and `GET /admin/models` reports only whether one is set.

### Providers

Most providers are OpenAI-compatible, so they need no code at all — just a
backend row pointing at their base URL. That includes **OpenRouter**, which
itself fronts Anthropic, Gemini and several hundred other models in OpenAI
format:

```bash
curl -X POST https://control/admin/models/$MODEL_ID/backends \
  -H 'content-type: application/json' -b "$SESSION" \
  -d '{"api_base":"https://openrouter.ai/api/v1",
       "upstream_model":"anthropic/claude-sonnet-4",
       "upstream_api_key":"sk-or-..."}'
```

Verified base URLs for the OpenAI-compatible set:

| provider | `api_base` |
|---|---|
| OpenRouter | `https://openrouter.ai/api/v1` |
| OpenAI | `https://api.openai.com/v1` |
| Groq | `https://api.groq.com/openai/v1` |
| DeepSeek | `https://api.deepseek.com/v1` |
| xAI | `https://api.x.ai/v1` |
| Together | `https://api.together.xyz/v1` |
| Fireworks | `https://api.fireworks.ai/inference/v1` |
| Nebius | `https://api.studio.nebius.ai/v1` |
| AtlasCloud | `https://api.atlascloud.ai/v1` |
| AIHubMix | `https://aihubmix.com/v1` |
| Z.ai | `https://api.z.ai/api/paas/v4` |
| BigModel | `https://open.bigmodel.cn/api/paas/v4` |
| Aliyun DashScope | `https://dashscope.aliyuncs.com/compatible-mode/v1` |
| Qwen Cloud | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` |
| Moonshot / Kimi | `https://api.moonshot.cn/v1`, `https://api.moonshot.ai/v1` |
| Baidu Qianfan | `https://qianfan.baidubce.com/v2` |
| GitHub Models | `https://models.github.ai/inference` |
| Ollama | `http://localhost:11434` |

**Anthropic and Gemini** speak their own wire formats and are reached by
setting `protocol`. The auth header and scheme are filled in automatically —
`x-api-key` plus `anthropic-version` for Anthropic, `x-goog-api-key` for
Gemini — so an operator sets neither:

```bash
# api_base already carries the version segment each vendor addresses from
-d '{"api_base":"https://api.anthropic.com/v1", "protocol":"anthropic",
     "upstream_model":"claude-sonnet-4-5", "upstream_api_key":"sk-ant-...",
     "default_max_tokens":4096}'

-d '{"api_base":"https://generativelanguage.googleapis.com/v1beta",
     "protocol":"gemini", "upstream_model":"gemini-2.5-flash",
     "upstream_api_key":"AIza..."}'
```

Two things to know before choosing native over OpenRouter:

- **`default_max_tokens` is required for Anthropic in practice.** Anthropic
  rejects a request with no `max_tokens`; a client that omits one gets a 400
  naming this field. It is deliberately not defaulted to an invented number —
  silently capping generation is the kind of bug nobody finds until they
  wonder why answers stop mid-sentence.
- **Translated backends serve `/chat/completions` only**, with text messages.
  Tool calling, images, `n > 1`, `logprobs`, `seed`, `response_format`, and
  the embeddings/rerank/audio endpoints all return `501` naming what was
  unsupported, rather than quietly doing less than was asked. Requests
  needing those should go to an OpenAI-compatible backend (OpenRouter serves
  the same models with tool calling intact).

Everything else is unchanged by the choice: RBAC, rate limits, budgets,
routing rules and virtual models all work the same against a translated
backend, and usage is reported from the provider's own token counts.

### Routing rules

A virtual model is a client-facing name with an ordered list of rules and a
fallback. First rule whose conditions match wins; conditions within a rule are
AND'd. Targets are weighted (relative shares, not percentages), and the target
list is a **fallback chain**, not just a split.

| condition | matches on | reads |
|---|---|---|
| `principals`, `roles` | who is calling | request |
| `min/max_prompt_tokens` | estimated prompt size | request |
| `min/max_max_tokens` | requested generation length | request |
| `stream` | whether the client asked for a stream | request |
| `headers` | exact header values, all must match | request |
| `min/max_budget_used_percent` | how much of the caller's budget is spent | snapshot |
| `max_inflight_per_backend` | how busy this rule's own targets are | **live cluster state** |
| `after`, `before`, `days`, `utc_offset_minutes` | wall-clock window | **clock** |

The last two rows are marked because they matter: every other condition is a
pure function of the request, so the same request always routes the same way
and prefix affinity means something. A load- or time-dependent rule gives that
up by design — two identical requests a second apart can legitimately land on
different models. Worth choosing knowingly.

Some shapes worth stealing:

```jsonc
// Burst to the cloud only when the local pool is full. First-match-wins does
// the work; there is no separate "spill" mechanism.
[{"position": 0, "max_inflight_per_backend": 2, "targets": ["local"]},
 {"position": 1,                                "targets": ["openrouter"]}]

// Let the client say what kind of work this is.
{"position": 0, "headers": {"x-fastllm-tier": "batch"}, "targets": ["cheap"]}

// Batch work (nobody is watching) goes somewhere slower.
{"position": 0, "stream": false, "targets": ["cheap"]}

// Degrade instead of refusing: past 80% of budget, use the free local model.
{"position": 0, "min_budget_used_percent": 80, "targets": ["local"]}

// Overnight, keep everything in-house. 22:00–06:00 local at UTC+2.
{"position": 0, "after": "22:00", "before": "06:00", "utc_offset_minutes": 120,
 "days": [1,2,3,4,5], "targets": ["local"]}
```

**Failover.** A rule's targets are tried in order. If the first model's whole
pool answers `5xx`, `429`, or cannot be reached, the request moves to the next
model in the same rule — before any byte has reached the client, so nothing is
corrupted. `429` counts because a hosted provider refusing a request is not
the same as being unhealthy: the pool passes every probe and still cannot serve
this call. When the chain is exhausted the last upstream's own status and body
reach the client rather than a synthetic 502.

Failover never widens reach: a candidate the caller lacks `model:invoke` on is
dropped from the chain, so a chain can span models with different grants
safely. Usage is attributed to the model that actually answered.

Malformed conditions (`"after": "25:00"`, `days: [8]`, a percentage above 100)
are rejected by `POST /admin/virtual-models/{id}/rules` with a message naming
the field, rather than stored as a rule that silently never matches.

### Admin authentication

Every `/admin/*` route (including `PUT /admin/principals/{id}/password` below) requires a valid session cookie, checked by `require_session` in `src/control/api.rs`. `POST /login` verifies a `{"name":..., "password":...}` body against `principals.password_hash` (Argon2id — see `src/control/auth.rs`'s doc comment for why this is a different hash from `api_keys.hash`'s SHA-256, deliberately: a password is low-entropy and human-chosen, an API key or session token is high-entropy random) and, on success, sets `fastllm_session` (`HttpOnly`, `SameSite=Strict`, `Secure` when TLS is on) valid for 12 hours. `POST /logout` deletes the session and clears the cookie.

`--proxy-token` still gates `/snapshot`, `/usage` and `/limits/reconcile` — those are proxy *processes* authenticating to the control plane, not humans, and have no password to present; sessions and the proxy token are deliberately separate mechanisms for separate callers.

**A session alone is not enough.** `require_session` only establishes *who* is calling; every `/admin/*` handler additionally checks *what* that principal may do, via `RequirePermission` (`src/control/api.rs`), against the same `roles → role_permissions → permissions` model `migrations/0001_init.sql` seeds and the data-plane's own `model:invoke` authorisation already uses. A session with no matching permission gets 403, not 200 — a principal that can log in is not, by that fact alone, an administrator. This closes what was previously a real gap: any principal a password was ever set for (via `PUT /admin/principals/{id}/password`) was a full admin, because nothing checked further than "is this a valid session".

Every admin route needs one of four permissions, seeded by `migrations/0001_init.sql`:

| Permission | Routes |
|---|---|
| `usage:read` | Every `GET /admin/*` route (keys, principals, models, virtual models, roles, limits, budgets, health) |
| `key:create` | `POST /admin/keys` |
| `key:revoke` | `DELETE /admin/keys/{id}` |
| `config:write` | Every other write: principals (create/delete/roles/**password**), models, backends, virtual models, routing rules and targets, limits, budgets |

There is no finer-grained permission for "manage principals" or "manage virtual models" than `config:write` — the schema does not seed one, and inventing a permission per table would multiply roles for no operator-visible benefit. The built-in `operator` role holds everything except `model:invoke` (i.e. all four of the above); `admin` holds everything including `model:invoke`. A role with `usage:read` alone can list and view but never create, revoke or reconfigure anything — the shape a read-only UI viewer or an audit tool needs.

**Bootstrapping the first login.** A freshly migrated database has no session anyone can obtain — every `principals` row starts with `password_hash IS NULL`. Run this once, with the same database access `import` already requires:

```bash
fastllm-proxy set-password --name admin --password '...' --database-url postgres://...
```

Creates the named principal if it does not exist yet (as `kind = 'user'`), sets its password, and grants it the `admin` role unless it already holds one granting `config:write` — the one way to reach every `config:write`/`key:create`/`key:revoke`/`usage:read` route before any session-driven role grant is possible. Safe to run again later to reset a forgotten password. `PUT /admin/principals/{id}/password` (session-gated *and* `config:write`-gated, for every login *after* the first) does the same password-setting step through the admin API/UI once at least one session exists — deliberately behind `config:write`, not merely a session: it is the route that hands a principal a working login, so only a caller already trusted to reconfigure the system may grant one to somebody else.

Network isolation is still the right default even with a login in front of `/admin/*` — the same reason a login-gated internal tool still belongs on an internal network rather than a public LoadBalancer. **Keep the admin port off the public listener**: bind it to a cluster-internal Service or localhost only. `deploy/control.yaml` does this with a ClusterIP Service kept off the LoadBalancer VIP; do not merge them.

### Management UI

`--role all`/`control` serve a small React dashboard from `/` and `/ui/*` — models and backends, virtual models and their rule chains, keys (create/revoke), principals/roles/grants, limits and budgets with consumption, and a usage overview, all driven by the admin API above (`src/control/ui.rs`; frontend source in `web/`). `--role proxy` serves no UI at all; `control::api::serve`, where the UI's fallback route is mounted, is never called for that role.

Embedded into the binary with [`rust-embed`](https://docs.rs/rust-embed) reading `web/dist/` at compile time — one container image, no second artefact to deploy. Built by the `Dockerfile`'s dedicated `node` stage, not a `build.rs` that shells out to `npm`, so `cargo build`/`cargo test` never require Node — a `web/dist/` empty at compile time (the normal state outside the Docker build) degrades to a plain "UI not available" response rather than failing the build. See `web/dist/.gitkeep`'s neighbour, `src/control/ui.rs`'s module doc comment, for the full mechanics.

### Encryption at rest

`model_backends.upstream_api_key` is encrypted at rest with AES-256-GCM (`src/control/secrets.rs`; `ring::aead`, already in the dependency tree via rustls) before `import`/the admin API ever write it to Postgres, and decrypted by `build_snapshot` when the control plane builds a snapshot. This protects the **database**, not the **snapshot**: `/snapshot` still carries the credential in usable plaintext form, because the proxy has to present it to the backend as a bearer token — an upstream credential cannot be reduced to a hash the way `api_keys.hash` is. `/snapshot` must be TLS wherever a backend has a real credential, exactly as before this existed. What encryption at rest actually buys: someone with read access to Postgres (a backup, a replica, a leaked `pg_dump`) no longer gets every upstream credential for free.

`--role control`/`all` and `fastllm-proxy import`/`reencrypt-backends` all require `FASTLLM_ENCRYPTION_KEY` — 32 bytes, hex-encoded (e.g. `openssl rand -hex 32`) — and refuse to start without it rather than falling back to plaintext. `--role proxy` never touches the database and never requires it. A database that already has plaintext rows from before this existed (a developer's scratch database — the live cluster has no control-plane database deployed at all yet) needs the one-shot `fastllm-proxy reencrypt-backends --database-url <url>` command run once; see `migrations/0004_encrypted_upstream_api_key.sql` for why this is a command rather than a format the read path silently tolerates forever.

### TLS on `/snapshot` and `/usage`

`/snapshot` carries `model_backends.upstream_api_key` in usable plaintext form (see "Encryption at rest" above), so it — and `/usage`, gated by the same token and sharing the same listener — must be TLS in any deployment where a backend has a real credential.

`--role control`/`all` take `--tls-cert`/`--tls-key` (PEM, `FASTLLM_TLS_CERT`/`FASTLLM_TLS_KEY`). Give both and the admin API — `/admin/*`, `/snapshot`, `/usage`, all of it, since they share one listener — serves HTTPS via `rustls`/`tokio-rustls` (already dependencies; no new TLS crate). Give neither and it serves plain HTTP, logging a startup warning every time it does, because a dev deployment with no real backend credentials is legitimate and must not be forced to generate a cert it does not need — but the fallback must never be silent. Giving only one of the two is a startup error, not a silent fall-back to HTTP.

On the client side, `--role proxy` in `Http` mode (`--control-url https://...`) and any `https://` backend `api_base` both go through the one pooled `Upstream` client (`src/upstream.rs`), which already speaks TLS. `--ca-bundle` (`FASTLLM_CA_BUNDLE`) adds one or more PEM CA certificates to the trust store *in addition to* the system roots — required to trust a private or self-signed cert (a cert-manager-issued, in-cluster control-plane certificate is the normal case; see `deploy/control.yaml`'s `fastllm-control-tls` `Certificate` and `deploy/README.md`'s TLS section) that no public root store contains. Without it, `--control-url https://...` against such a cert fails the handshake.

### `POST /usage`: the reverse channel

`--role proxy` holds a `usage::UsageReporter`: a bounded queue plus a background flush task that batches events and posts them to `/usage` on its own schedule, authenticated with the same `--proxy-token` as `/snapshot`. Recording an event (`UsageReporter::record`) is a non-blocking `try_send` — a full queue means the control plane is not keeping up, and the event is dropped rather than applying backpressure to inference (the design's stated tradeoff: "dropping usage rather than blocking a request is deliberate — billing accuracy is not worth failing inference"). Dropped events are counted and exposed on `/metrics` as `fastllm_usage_reports_dropped_total`, so the loss is visible instead of silent. `--role all` loops this same request back to its own admin API over `127.0.0.1` rather than inventing a second, in-process delivery path.

On the control-plane side, `POST /usage` accepts a batch, persists it to `usage_events`, and folds each event's tokens into its principal's `budgets.tokens_used`, if that principal has a configured budget. A record naming a `principal_id` or model that no longer exists is dropped from the batch rather than failing the whole request — one stale id from one replica must not poison every other principal's usage in the same flush interval.

### P3: usage accounting and budgets

The proxy never parses response bodies — that is its whole performance story — but usage lives in the body, so this is the one place that comes under tension. The resolution:

- **Getting the numbers.** A non-streaming response already carries a top-level `usage` object. A streaming one only does if the request set `stream_options.include_usage`, so `src/proxy.rs`'s `rewrite_model_if_needed` injects that field into the upstream body — reusing the same body-rewrite path that already exists for model aliases — but **only** for a principal with a configured budget or a `tokens_per_min` limit (`principal_needs_usage`), never globally, because the injection adds a usage chunk the client did not ask for.
- **Reading them without becoming a parser.** `src/tail_buffer.rs`'s `TailBuffer` keeps a fixed-size (8 KiB) ring of the last bytes forwarded — `TrackedBody::poll_frame` in `src/proxy.rs` mirrors every frame into it with a memcpy, never a parse, alongside the pass-through that already exists. At clean end of stream, and only then, the tail is parsed once for a trailing `usage` object (streaming SSE or non-streaming JSON, tried in that order) and the result — or nothing, if none was found, which is an ordinary outcome, not an error — is handed to `usage::record`. A response larger than the buffer, or a stream that ends mid-frame, still costs exactly one bounded parse and never panics.
- **Enforcement is after the fact.** `Principal.budget` is pre-resolved into the snapshot (`control::build::roll_over_and_load_budgets`), so the request path's check is one integer comparison, no I/O — the same shape as the rate limiter just above it. A request that pushes a principal over budget still completes; the *next* one is refused, with **402 Payment Required**, not 429: rate limiting (429) is a pacing problem where waiting a few seconds fixes it, and `Retry-After` says how long; a budget is a spending problem where no amount of waiting helps until the window rolls over or an operator raises the limit, which is what 402's stated meaning — access denied pending an accounting action — actually matches.
- **Budgets roll over.** `daily`/`weekly`/`monthly` (fixed-length — 1/7/30 days, not calendar-month arithmetic) checked on every snapshot rebuild; an elapsed window resets `tokens_used` to zero and persists the new `window_start`, advancing exactly one window forward even if several were missed while nobody looked.

### Rate limits

Limits attach to a principal, not a key: `requests_per_min` and `tokens_per_min`, either or both, set via `PUT /admin/principals/{id}/limits` (or, in `File` mode, a `limits:` block under an `auth.keys` entry — see "Per-key RBAC in `File` mode" below). A principal with no configured limit is unlimited, not limited to zero.

Enforcement is a local token bucket per principal per replica (`src/limiter.rs`): one hash lookup and a short, synchronous decrement, no I/O, no allocation once the principal's bucket exists. `tokens_per_min` is charged against an estimate — the same prompt-size estimate the P1 routing rules use, plus the client's requested `max_tokens` — since actual usage is not known until the response completes. Exceeding either dimension answers `429` with `Retry-After` (whole seconds).

A single replica enforcing the full configured limit locally would, with N replicas, admit N× the intended traffic. Accuracy comes from periodic **reconciliation** instead of a shared counter: every `--rate-limit-reconcile-interval` seconds (default 5) each `--role proxy` reports its locally observed request/token counts to `POST /limits/reconcile`, which aggregates across every replica that has reported recently and returns each one's *share* — proportional to how much of a principal's traffic it is actually handling — of that principal's configured limit for the next window. The reporting client (`src/reconcile.rs`) reuses the same pooled `Upstream` client and bearer-token pattern as `POST /usage`, but it is not fire-and-forget like that route: the whole point is the allowance in the response body. Before a replica's first successful reconciliation (at startup, or after a control-plane outage) it enforces the *full* configured limit locally, which is the design's accepted cost: **a limit can be exceeded by up to one reconciliation window's worth of traffic during a sharp spike**, in exchange for never putting a network round trip on the request path.

`--role all` never spawns the reconciliation client at all — one process's local counters already are the global counters, so there is nothing to reconcile, and the machinery is inert rather than merely harmless: no background task, no timer, no socket.

### Options

| Flag | Default | Notes |
|---|---|---|
| `--config` | *required* | LiteLLM-format YAML |
| `--host` / `--port` | `127.0.0.1` / `4000` | Loopback by default; bind wider deliberately |
| `--master-key` | from config | Bearer token required from clients |
| `--policy` | `cache-affinity` | Or `least-loaded`, `round-robin` |
| `--max-retries` | `2` | Alternate backends tried before any bytes reach the client |
| `--upstream-timeout` | `120` | Seconds to *first byte*. Does not bound generation |
| `--health-interval` | `10` | Seconds between probes |
| `--max-body-mb` | `64` | Request body ceiling |
| `--pool-max-idle` | `256` | Idle upstream connections per backend |
| `--workers` | core count | Tokio worker threads |
| `--rate-limit-reconcile-interval` | `5` | Seconds between rate-limit reconciliation reports in `Http`-mode `--role proxy`. `0` disables it |

## Configuration

The schema is a superset of the LiteLLM proxy config, so a file generated by `sparkrun proxy start` works as-is:

```yaml
model_list:
  # Two entries sharing a model_name become one load-balanced pool.
  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.24.11.13:8000/v1
      api_key: not-needed
  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.24.11.14:8000/v1

  # An alias: clients say "gpt-4", the upstream is sent its real name.
  - model_name: gpt-4
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.24.11.13:8000/v1

general_settings:
  master_key: sk-...

# Optional, ignored by LiteLLM so one file can drive either.
fastllm:
  prefix_bytes: 2048      # bytes of the raw body hashed for the affinity key
  balance_abs: 8          # absolute in-flight slack before affinity yields
  balance_rel: 1.5        # relative slack multiplier
  affinity_slots: 65536   # prefix-affinity cache entries
  unhealthy_after: 2      # consecutive failed probes before eviction
```

`openai/`, `vllm/`, `hosted_vllm/` and `openai_like/` prefixes are stripped from `litellm_params.model`; a name that is genuinely `Qwen/Qwen3-1.7B` keeps its org. `not-needed`, `none` and `null` API keys are treated as absent.

### Per-key RBAC in `File` mode

`--master-key`/`general_settings.master_key` is one shared secret for every client and is deprecated. The replacement in `File` mode (no `--control-url`) is an `auth:` block:

```yaml
auth:
  keys:
    - key: sk-...
      name: ci-pipeline
      models: ["qwen3-6-35b-a3b-nvfp4"]   # `["*"]` for every model; an empty
                                          # or omitted list grants nothing
      expires_at: "2027-01-01T00:00:00Z"  # RFC 3339, optional
      limits:                             # optional; absent means unlimited
        requests_per_min: 60
        tokens_per_min: 100000
      budget:                             # optional; absent means unlimited
        tokens_total: 1000000
        tokens_used: 0                    # optional starting point; static —
                                           # File mode has no reconciliation
                                           # loop to advance it on its own
```

Absent `auth:` means open (no key required) — today's behaviour when no master key is set either. In `Http` mode (`--control-url` given), `auth:` is ignored: keys live in the database and are managed through the control plane's admin API instead. `fastllm-proxy import` carries an existing `auth:` block into that database unchanged (see "Migrating a `File`-mode deployment" above), so the same keys authorise the same models on either side of the move. `limits` is `File` mode's mirror of the control plane's `limits` table (see "Rate limits" above) — either field alone, both, or neither. `budget` is the same mirror of the `budgets` table (see "P3: usage accounting and budgets" above).

### Tuning affinity

`balance_abs` / `balance_rel` set how much imbalance is tolerated before cache locality is given up. Higher values favour cache hits; lower values favour even load. The default (8 requests absolute, 1.5× relative) suits a small cluster of a few nodes with long shared system prompts. If your traffic has little prefix sharing, `--policy least-loaded` is the honest choice and skips the bookkeeping.

## Behaviour notes

- **Retries** only happen before any byte has been forwarded. Once the response is committed a mid-stream failure propagates as-is — it cannot be silently retried without corrupting the stream.
- **5xx is retried, 4xx is not.** A client error retried across every node is the same client error three times.
- **The last backend's response is forwarded verbatim.** A 5xx is only retried while another backend remains; when none does, the upstream's own status and body reach the client rather than a synthetic 502. On a single-node pool that means every error keeps the engine's diagnostics.
- **Audio endpoints take `multipart/form-data`.** `model` is read from the form field and the upload is forwarded byte for byte, content-type and boundary intact. An alias splices the new name into that one field rather than re-encoding the body.
- **`https://` backends work**, so a TLS-terminated or hosted endpoint can sit in the same config as cluster-local nodes. System root certificates are used, falling back to the bundled Mozilla set.
- **A backend that fails every probe is still used as a last resort** rather than returning 503. A stale health flag should not turn a recoverable request into an outage.
- **The client's `Authorization` header is never forwarded.** It authenticates the client to the proxy; the upstream gets the backend's own key or none — in whichever header that provider reads it from.
- **An OpenAI-compatible backend's response is never parsed.** Bytes are forwarded verbatim, which is why proxied overhead measures at zero; `tests/native_protocols.rs` pins it against an intentionally odd-but-valid payload. Only a backend explicitly configured for a native protocol is translated, and only there is a response body read.
- **A backend's identity covers its whole configuration.** Rotating an upstream key, or changing a backend's protocol, produces a new routing entry rather than reusing the live one — otherwise a reload would keep serving with the old credential, since backend objects are carried across reloads to preserve their in-flight counts.
- **Affinity keys hash the raw request prefix**, not parsed fields. JSON does not guarantee field order, but order is stable per client, which is all affinity needs — a client that reorders per request degrades to least-loaded rather than misrouting.
- **A rate-limited request gets `429` with `Retry-After`**, checked after authorisation and model resolution but before the request is dispatched upstream — nothing is forwarded on a rejected request. See "Rate limits" above.

## Development

```bash
cargo test
cargo build --release
```

## License

Apache-2.0
