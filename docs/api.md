# API and administration

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
| `POST /v1/chat/completions` | Proxied byte-for-byte. Also `/completions`, `/responses`, `/embeddings`, `/rerank`, `/score`, `/audio/transcriptions`, `/audio/translations`, `/audio/speech`, `/images/generations`, `/images/edits`, `/moderations` |
| `GET /v1/models` | Aggregated across every pool |
| `GET /health` | Per-backend health, in-flight, request and error counts, plus `snapshot_version` and the key count for the configuration this process is serving. No auth required. Exposes backend addresses — keep it off the public interface |
| `GET /metrics` | Prometheus text, including `fastllm_snapshot_version`. No auth required |
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
| `GET /admin/prompt-classes` | Classes, example counts, and whether each is *routable* (has a centroid) |
| `POST /admin/prompt-classes` | `{"name":..., "tier":"fast"\|"refined", "min_margin":..., "refines":[...], "examples":[...]}` |
| `POST /admin/prompt-classes/{id}/examples` | Add one example prompt |
| `DELETE /admin/prompt-classes/{id}` | Cascades to its examples and refinements |
| `POST /admin/prompt-classes/evaluate` | Per-class precision, recall, margins, nearest neighbours and a verdict — leave-one-out over your own examples |
| `GET /admin/fallback-model` | The model every routing chain falls back to |
| `PUT /admin/fallback-model` | `{"model_id": 42}` to set it, `{"model_id": null}` to clear it |
| `GET /admin/roles` | Roles and the permissions each one grants |
| `GET /admin/limits` | Every principal with a configured rate limit |
| `PUT /admin/principals/{id}/limits` | `{"requests_per_min":..., "tokens_per_min":...}`. Either or both; upserts the one row this principal may have |
| `DELETE /admin/principals/{id}/limits` | Remove the limit — the principal becomes unlimited, not limited to zero |
| `GET /admin/budgets` | Every principal with a configured token budget, including current consumption |
| `PUT /admin/principals/{id}/budget` | `{"tokens_total":..., "window":"daily"\|"weekly"\|"monthly"}`. Upserts the one row this principal may have; leaves `tokens_used` and the window's start alone on an update |
| `DELETE /admin/principals/{id}/budget` | Remove the budget — the principal becomes unlimited, not limited to zero |
| `PATCH /admin/models/{id}` | Correct a model in place: `{"description":..., "input_price_per_mtok":..., "output_price_per_mtok":..., "cache_ttl_seconds":...}`. Every field optional; an explicit `null` clears, an absent field is left alone |
| `POST /admin/roles` | `{"name":..., "description":...}`. Permissions attach to roles, so a role is the only place to express "this caller may reach these models and nothing else" |
| `DELETE /admin/roles/{name}` | Refused while any principal still holds it — a cascade would take every holder's access away at once, and the symptom arrives long after the click |
| `POST /admin/roles/{name}/permissions` | `{"verb":"model:invoke", "resource":"model/gpt-4o"}`. The verb list is closed — a permission nothing checks would read on a matrix as though it granted something |
| `DELETE /admin/roles/{name}/permissions` | Same body; revoke one |
| `GET /admin/audit` | The change log, newest first. `?limit=&before=&actor_id=&target=&since=`. `before` is keyset pagination on the id of the oldest row you hold — an offset would skip or repeat rows as new ones arrive at the head |
| `GET /admin/usage` | Aggregate requests, tokens, latency and spend. `?group_by=model\|principal\|virtual_model\|day&since=&until=&limit=`. `virtual_model` groups on what the caller *asked for*, which is the only grouping that can answer "how much traffic does each virtual model carry" — by the time a model is chosen the virtual name is gone. Reports `unpriced_requests` alongside every total: a request whose model has no price contributes nothing to `cost`, and summing those as zero would understate spend silently |
| `GET /admin/fleet` | What each proxy replica can see — its backends' health, in-flight counts, and the snapshot version it is serving |
| `POST /admin/routing/dry-run` | `{"model":..., "streaming":..., "principal_id":..., "class":..., "headers":{...}}` → the candidate chain and **which rule index decided** |
| `POST /admin/prices/sync` | `{"source":"open-router"\|"catalogue"\|"both", "overwrite":..., "dry_run":...}`. The same work `fastllm-proxy sync-prices` does, from a UI |
| `GET /admin/config` | What this process was started with — role, TLS, poll and report intervals, cache bounds, session TTL, classifier tiers, OTLP. Read-only: changing one of these is a deploy |
| `POST /admin/snapshot/rebuild` | Rebuild and republish now. Answers with the version it published, because `refresh` deliberately does not fail the request that triggered it |
| `POST /admin/sessions/revoke-all` | Delete every session, including the caller's |

**No route returns a credential.** Key plaintext is shown once, by `POST /admin/keys`, and never again; `api_keys.hash` is a verifier, not a display value, and is not in any response. `upstream_api_key` is the one secret that cannot be reduced to a hash — the proxy has to present it upstream — so it is encrypted at rest and `GET /admin/models` reports only whether one is set.

### Endpoints, and what is not one

Twelve `POST` endpoints are proxied. All of them take the same path: read
`model` from the body, authorise it, route it, forward the bytes. Nothing on
that list is parsed on the way back, so adding one costs a line — which is why
`/responses`, `/audio/speech`, `/images/*` and `/moderations` are there.

A **native** (`anthropic`/`gemini`) backend answers `501` for everything except
`/chat/completions`, because only chat has a translation. That gate is what
makes adding a passthrough endpoint safe: a native backend refuses it clearly
instead of being handed a body it cannot read.

**What is deliberately absent, and why it is not a line of config.** The
stateful job APIs — `/batches`, `/files`, `/fine_tuning` — are not endpoints so
much as small databases. Creating a job is a `POST` with a `model` in it, which
would work; *retrieving* one is a `GET /v1/batches/{id}` with no model and no
body, so there is nothing to route on. Serving them means remembering which
backend owns which job id, which is durable state on the request path — the one
thing this proxy is built not to have. They need a design, not a suffix.

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
| Cohere | `https://api.cohere.ai/compatibility/v1` |
| Amazon Bedrock | `https://bedrock-runtime.<region>.amazonaws.com/openai/v1` |
| Google Vertex AI | `https://<region>-aiplatform.googleapis.com/v1/projects/<project>/locations/<region>/endpoints/openapi` |

**Bedrock** needs no request signing. Its OpenAI-compatible endpoint takes a
Bedrock API key as an ordinary bearer token, so it is a plain backend row like
any other — create the key in the Bedrock console and put it in
`upstream_api_key`.

### Response cache

Off unless a model asks for it:

```bash
-d '{"name":"embeddings","cache_ttl_seconds":300}'
```

An identical request to that model — same resolved model, same body — is
answered from memory without touching the provider. Responses carry
`x-fastllm-cache: hit` or `miss`, because a caller measuring latency deserves
to know why one request took a microsecond and the next took a second.

Opt-in per model rather than global, because caching changes semantics: two
identical requests at `temperature > 0` are supposed to be able to differ. A
deployment that sets nothing pays nothing, not even the hash — that is only
computed once a model is known to have caching on.

**Non-streaming 2xx responses only.** Caching a stream would mean buffering the
whole response before any of it reached the client, turning the one path this
proxy exists to keep incremental into a batch operation. Errors are never
cached: a 429 is a statement about *now*, and serving it from cache would keep
a provider's bad minute alive long after it ended. The natural fit is
embeddings and short completions, which are the requests that repeat.

The cache is **per process**, bounded by `--cache-max-entries` and
`--cache-max-bytes` (both matter: a thousand embedding responses is nothing and
a thousand completions is hundreds of megabytes). A shared cache would mean a
network call, and the request path performs no I/O — a lower hit rate across
replicas is the honest cost of that invariant.

A cache hit still counts against the caller's rate limit and budget. A cache is
a latency and cost optimisation, not a way around a quota. And the whole cache
is dropped whenever a snapshot changes, since a reconfiguration can repoint a
model at a different provider and there is no way to tell from a key which
entries are affected — a cold cache is a latency cost where a stale one is a
correctness bug.

### Audit log

`usage_events` records inference. `audit_events` records the other kind of
action — who created a key, granted a role, raised a budget, repointed a
backend at a different provider. Those are the changes an incident review asks
about.

```sql
SELECT at, actor_name, action, target FROM audit_events ORDER BY at DESC LIMIT 20;
```

Recorded by a layer over every `/admin/*` route rather than by a call in each
handler, and that is the point: a hand-wired trail records the mutations
somebody remembered to wire, which drifts the moment a route is added. A new
endpoint is audited before it is written.

What that costs is detail — the row says a principal's roles were changed and
by whom, not which role. Complete and coarse beats detailed and full of holes,
and the application log carries the rest.

Three things are deliberately absent. **Reads** are not recorded: auditing
every list call would bury the changes in noise — including the handful that
are `POST` only because they take a body (`/admin/routing/dry-run`,
`/admin/prompt-classes/evaluate`), which would otherwise dilute a log whose
value is that every row is a change. **Rejected attempts** are not
recorded as changes: a 403 is an attempt, and logging it as a change would make
the trail lie in the direction that matters most. And the **request body** is
never captured — it carries passwords and upstream credentials, and an audit
row is read by more people than the thing it describes.

A failed audit write never fails the request. Losing a row is serious; losing
the change as well would be worse, since an operator retrying a failed grant
would have no way to tell whether the first attempt applied.

### Live backend health

Backend health lives in the data plane: each proxy probes its own backends and
keeps its own in-flight counts. The control plane has never seen any of it — it
publishes a snapshot and hears back only about usage. So `GET /admin/fleet`
exists, fed by proxies posting to `POST /health-report` on the same
`--proxy-token` as `/snapshot` and `/usage`, every `--health-report-interval`
(10s by default).

Reports are kept **per replica and never merged**. The interesting failures are
exactly the ones where replicas disagree: one proxy that cannot reach a backend
the others can is a network partition, and averaging it into a fleet-wide
"healthy" hides the only symptom there is. Each report also carries the
snapshot version that replica is serving, so a fleet-wide `max - min` shows a
pod stuck on an old configuration without scraping every one of them.

Nothing is persisted. Health is a statement about *now*; a row saying a backend
was up two hours ago is history, not health. A replica that stops reporting
ages out after 30 seconds rather than lingering as "up, 40 minutes ago".

### Routing dry-run

`POST /admin/routing/dry-run` answers the question a rule author actually has —
"does my `coding` rule fire for this caller?" — without sending a real request
and reading the answer out of a log. It returns the candidate chain and the
index of the rule that decided, because "my second rule matched instead of my
first" and "my first rule matched and points somewhere I did not expect" are
different bugs with the same symptom.

Two honest limits. Backend **health is not consulted**: the registry is built
fresh from the snapshot, so every backend looks up — `GET /admin/fleet` is
where reachability lives. And the prompt **class is supplied, not computed**,
so this tells you what a `coding` prompt would do, not whether some particular
prompt is coding — `POST /admin/prompt-classes/evaluate` answers that one.

### Rate limit headers

Every response from a principal with limits configured carries the de-facto
`x-ratelimit-*` shape, so a client that already paces itself against OpenAI
needs no new code:

```
x-ratelimit-limit-requests / x-ratelimit-remaining-requests
x-ratelimit-limit-tokens   / x-ratelimit-remaining-tokens
x-ratelimit-reset
```

Remaining is floored, not rounded — 0.6 of a request is not one a client can
spend. `x-ratelimit-reset` is seconds until the allowance is fully back; a
token bucket has no discrete window to reset, so that is the honest reading,
and a full bucket reports 0. A principal with no limits gets no headers at all,
because publishing `remaining: 0` to an unlimited caller would make a
well-behaved client back off against a limit that does not exist.

### Retries

A retry waits 25ms, then 50ms, then 100ms, plus up to 50% jitter, and doubles
that for a 429 — a provider that just said "too many requests" means it.
Bounded deliberately: the delay is paid by a client still waiting for its
answer, so this is a retry budget measured against one request's patience, not
a background job's. Jitter is keyed on the request rather than an RNG, since
the data plane has no random source in a `--no-default-features` build and all
that matters is that simultaneous retries decorrelate.

### Cost

Models carry a price per **million** tokens, in micro-units of whatever
currency you quote in — an integer in the smallest unit anyone publishes, so
the arithmetic is exact and there is no rounding mode to get wrong. `3000000`
is $3.00 per million tokens.

```bash
-d '{"name":"claude-sonnet","input_price_per_mtok":3000000,"output_price_per_mtok":15000000}'
```

Nobody needs to type them:

```bash
fastllm-proxy sync-prices --database-url "$URL" --dry-run
```

Reads OpenRouter's model list (395 models, unauthenticated) and the community
catalogue (2,499), matches each model by what its backends call upstream —
trying `openai/gpt-4o` and then `gpt-4o`, so you need not know which spelling a
source uses — and fills in the prices. `--source` picks one; `--overwrite`
replaces prices already set, which it will not do otherwise: a negotiated rate
should not be replaced by a list price on the next run. A source that cannot be
reached is reported and skipped, since filling in half the prices beats filling
in none because GitHub was briefly unavailable.

Where the two disagree, OpenRouter's own published price wins over a third
party's copy of it. And the catalogue is a third party's file — correct in
practice, occasionally stale, and a dependency on somebody else's maintenance.

Prices are changed in place, and read back:

```bash
curl -X PATCH .../admin/models/42 -d '{"input_price_per_mtok":4000000}'
```

Absent means "leave alone" and an explicit `null` clears — so correcting a
price does not silently turn caching off, and a model can become unpriced
again. `GET /admin/models` returns both prices and the cache TTL.

**The provider's own figure wins where it gives one.** OpenRouter returns
`usage.cost` unasked, and that is authoritative: it is the amount actually
billed, it already accounts for cache discounts and for a routed alias serving
a different model per request, and it does not go stale when a provider changes
its prices. The configured price is the fallback, not the source. Most
providers report nothing, and those are priced from the table.

Every usage row carries `cost_micros`, stored rather than derived — a later
price change must not silently rewrite what last month cost. A model with no
price and no reported cost is left NULL rather than zero, so unpriced is
visible instead of looking free. The table fallback rounds rather than
truncating: a small request often costs single-digit micro-units, and
truncating each one undercounts systematically rather than symmetrically.

Budgets cap tokens, money, or both:

```bash
curl -X PUT .../admin/principals/42/budget \
  -d '{"cost_total_micros":500000000,"window":"monthly"}'   # $500/month
```

A request is refused when **either** cap is reached, and the 402 names which
one — "budget exhausted" alone leaves an operator guessing between raising
tokens and raising spend. Both counters roll together at the window boundary,
since they measure the same window.

`min/max_budget_used_percent` routing conditions read whichever cap is closest
to its limit, so a rule meant to degrade before the cliff still fires for a
principal running out of money rather than tokens.

**Checking a proxy is current.** `snapshot_version` on `/health` — and
`fastllm_snapshot_version` on `/metrics` — is the version of the configuration
that process is actually serving, stamped by the control plane and so
comparable across a fleet. A `max() - min()` across proxies that is not zero
for more than a poll interval means a pod is stuck on an old configuration.
This matters because a lagging proxy is otherwise invisible: it answers
`/health` with `ok`, lists the right models and backends, and misbehaves only
on whichever part of the snapshot changed — most often a key it has never seen,
which looks to the caller like an invalid key rather than a stale proxy.

**Vertex AI** is the one provider that cannot be reached with a static secret:
it wants an OAuth2 access token, and those expire hourly. Give it the service
account's JSON key file and say so:

```bash
-d '{"api_base":"https://europe-west1-aiplatform.googleapis.com/v1/projects/my-project/locations/europe-west1/endpoints/openapi",
     "upstream_model":"google/gemini-2.5-flash",
     "credential_kind":"gcp_service_account",
     "upstream_api_key":"<the whole service-account JSON key file>"}'
```

The control plane exchanges the key file for an access token while building
each snapshot, caches it until five minutes before expiry, and ships the
*token*. The data plane never learns this backend is different — it presents a
bearer credential exactly as it would a static one, and performs no I/O to
obtain it. A key file that is not one is rejected when the backend is created,
rather than becoming a backend that disappears from routing on the next
rebuild. If minting fails later — a revoked key, a role removed — that one
backend drops out with the reason logged, and every other model keeps serving.

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
- **Translated backends serve `/chat/completions` only.** Text and tool
  calling work, streaming included — `tools`, `tool_choice`, `tool_calls` and
  `role: "tool"` messages all translate, in both directions, as do image and
  audio content parts and `response_format`. `n > 1`, `logprobs`, `seed`, the
  deprecated `functions` parameter, and the embeddings/rerank/audio endpoints
  return `501` naming what was unsupported, rather than quietly doing less
  than was asked. Requests needing those should go to an OpenAI-compatible
  backend.

  **Structured output translates, with one asymmetry.** A `json_schema`
  becomes Anthropic's `output_config.format` and Gemini's
  `generationConfig.responseSchema`. A bare `{"type":"json_object"}` — JSON
  with no schema — maps to Gemini's `responseMimeType` but is dropped for
  Anthropic, which has no equivalent: an empty schema there would constrain
  the model to `{}`.

  **Anthropic prompt caching is switched on for you.** Anthropic caches
  nothing unless a block carries `cache_control`, and a cache hit costs 90%
  less than the same input tokens — but an OpenAI-format client has no way to
  ask for it, so a translated backend paid full price on every request for a
  prefix identical across all of them. The system prompt now carries the
  breakpoint. It goes there and nowhere else: the system prompt is the one
  part of a chat request that is stable across turns by construction, where
  marking a message would be guessing at which prefix repeats.

  **Media never causes a fetch.** A `data:` URL carries the bytes inline and
  translates exactly, base64 untouched. A remote `https://` URL is handed to
  Anthropic, which fetches it itself; for Gemini it is a `501` naming the fix,
  because `fileData.fileUri` only addresses Google's own Files API. The proxy
  does not download it in either case — that would be a network call while
  serving a request, which `tests/no_io_on_hot_path.rs` forbids. Audio reaches
  Gemini as `inlineData`; Anthropic has no audio input, so it is a `501` rather
  than an image block with an audio media type that fails upstream.

  Two details a client can observe. Gemini supplies no tool-call id, so the
  proxy synthesises one — stable within a response, which is all a client
  needs to pair a result back to its call. And a Gemini call arrives complete
  in a single streamed frame where Anthropic's arguments accumulate across
  several; both are valid OpenAI streams, and a client that concatenates
  `arguments` by `index` handles either without knowing which provider
  answered.

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
| `class` | which prompt class the classifier assigned — see [semantic routing](classifier.md) |
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

`--role all`/`control` serve a React dashboard from `/` and `/ui/*` (`src/control/ui.rs`; frontend source in `web/`). `--role proxy` serves no UI at all; `control::api::serve`, where the UI's fallback route is mounted, is never called for that role.

Thirteen screens, all driven by the admin API above: **Overview** (fleet, backends, traffic, recent changes), **Metrics**, **Usage & spend**, **Providers**, **Models**, **Virtual models** with the routing dry-run, **Prompt classes** with the leave-one-out evaluation, **API keys**, **Principals & roles** with the permission matrix and per-model grants, **Limits & budgets**, **Audit log**, **Fleet**, and **Settings**.

**Nothing on a screen is invented.** Where the control plane cannot answer a question, the UI says so and names what can: per-backend latency percentiles are per process and do not merge, so the Metrics screen prints the `histogram_quantile` query rather than an average of p99s; a model with no price shows `unpriced`, never `$0.00`; a backend no replica has probed shows a grey dot, not a green one. The rule is the same one the docs follow — a number nobody can reproduce is worse than an absent one.

The one thing that *is* computed in the browser is a rate: the control plane stores no metric history, so every line on the Metrics screen is a delta between two polls of the counters the fleet reports, starting empty when the page loads. The header says so on the page.

Three checks guard it, all under `web/` (`npm test`, plus a CI job and the Dockerfile's web stage):

- **`test/render.mjs`** mounts every screen against stubbed responses and fails on a render error or missing content. `npm run build` proves the modules parse; it says nothing about whether a screen renders, and a component used but not imported is a clean build and a blank page.
- **`test/interact.mjs`** clicks every control on every screen (231 of them) and then asserts the exact method, path and body that the important mutations send. This exists because the worst bug this UI has had was a screen that rendered perfectly while posting `{position, match_condition: {...}}` to a handler that flattens the conditions — serde discarded them, answered 201, and every rule created through the UI matched every request. Nothing looked wrong; only the request body was, and no test had ever looked at one.
- **`test/browser.mjs`** (`npm run test:browser`, needs a running control plane) drives the built bundle in headless Chrome: a real login, every screen, the dry-run against the live routing engine, and a second pass at 1280px. jsdom does no layout at all — every element is zero by zero and nothing can overlap — so the entire visual half of the UI was unverified by the two harnesses above. This one checks what only a browser knows: console errors, failed requests, sideways page scroll, zero-size or clipped controls, text overflowing its box. It writes a screenshot per screen to `web/.screenshots/` to be looked at, and launches Chrome with its own `--user-data-dir` so it never touches a browser already open.
- **`test/verify-fixtures.mjs`** (`npm run test:fixtures`, needs a reachable control plane) compares the fixtures against a live API. Both harnesses are only as truthful as their fixtures, and twice a fixture written from the Rust *field name* rather than the wire format hid a real bug while the suite stayed green — the flatten above, and `model:invoke` with resource `*` where the API stores `model/*`.

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

### `POST /health-report`: the same channel, one more fact

`--role proxy`/`all` also post a health report every `--health-report-interval`
(`FASTLLM_HEALTH_REPORT_INTERVAL`, default 10s) on that same `--proxy-token`
channel, read back by `GET /admin/fleet` (see "Live backend health" above).
Same posture as usage: a bounded queue — depth one, since only the newest
report says anything true — and a failed delivery is dropped rather than
retried or allowed to block anything. `--role all` loops it back over
`127.0.0.1` exactly as usage does.

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

---

Back to the [README](../README.md).
