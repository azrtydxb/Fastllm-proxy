# Admin API

Everything under `/admin/*` on the control plane: models, backends, keys,
principals, prices, health and the audit log.

Everything an operator needs to run the control plane, so that neither raw SQL nor a second `import` run is the documented way to change policy. Every mutating route rebuilds and republishes the snapshot on the spot, so a change reaches `--role proxy` within one `--config-poll` interval rather than waiting on the control plane's own periodic rebuild.

| Endpoint                                     | Purpose                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| -------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET /admin/principals`                      | Principals with their roles                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `POST /admin/principals`                     | `{"name":..., "kind":..., "email":...}`. `kind` is `service_account` (the default) or `user`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `DELETE /admin/principals/{id}`              | Cascades to that principal's keys and role grants                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `POST /admin/principals/{id}/roles`          | `{"role":"inference"}`. Idempotent                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `DELETE /admin/principals/{id}/roles/{role}` | Revoke one role                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `PUT /admin/principals/{id}/password`        | `{"password":...}`. Argon2id-hashes it and promotes the principal to `kind = 'user'` if it was not already                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `GET /admin/keys`                            | Prefix, name, principal, expiry, disabled. **Never** the key or its hash                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `POST /admin/keys`                           | `{"name":..., "principal_id":..., "expires_at":...}`. Returns the plaintext key once                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `DELETE /admin/keys/{id}`                    | Revoke (sets `disabled`; the row stays for audit)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `GET /admin/provider-models`                          | Models, each with the provider serving it. Reports _whether_ the provider has an upstream credential, never the credential. A model with no provider is listed with an empty `backends` — not routable, and shown rather than hidden                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `POST /admin/provider-models`                         | `{"name":..., "description":...}`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `DELETE /admin/provider-models/{id}`                  | Deletes the model. The provider stays: other models may be on it                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `POST /admin/provider-models/{id}/backends`           | Attach the model to a provider. Either `{"provider_id": 7}` — the address, protocol and credential come from that row, and any field describing an endpoint alongside it is a **400** rather than silently ignored — or the address itself: `{"api_base":..., "upstream_model":..., "upstream_api_key":..., "protocol":..., "auth_header":..., "auth_scheme":..., "default_max_tokens":...}`. Everything after the credential is optional and defaults to an OpenAI-compatible upstream reached with `Authorization: Bearer`. The provider is reused when one already serves that `api_base` and auth, so a credential is stored once however many models ride on it, and is encrypted before it reaches Postgres and cannot be read back. **409 if the model already has a provider** — a provider model has exactly one; two upstreams behind one client-facing name is a frontend model with two targets                                                                                                                                                                                                                                                                              |
| `DELETE /admin/backends/{id}`                | Detach a model from its provider. The id is the **model's** — the model is its own link. Price, usage history and any frontend model pointing at it are untouched; it simply stops being routable                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `GET /admin/providers`                       | Every provider: its kind (`static`, `cloud`, `dynamic`), endpoint, protocol, whether a credential is set, the `node` vouching for a dynamic one, and how many models ride on it. A provider used to be a grouping the UI invented at render time; it is a row |
| `POST /admin/providers`                      | `{"name":..., "kind":..., "catalogue_key":..., "api_base":..., "protocol":..., "auth_header":..., "auth_scheme":..., "upstream_api_key":..., "credential_kind":...}`. All optional but for an address, which `catalogue_key` can supply. `kind` is `static` or `cloud`; `dynamic` is refused, because a lease is something a registering host takes out and keeps refreshing. A catalogue base URL with a `<placeholder>` still in it is a **400**: storing it would produce a provider that resolves nowhere and then reports itself unreachable. **409** on a duplicate name, or on a second provider at the same address with the same auth — that tuple is how attaching a backend finds one |
| `PATCH /admin/providers/{id}`                | `{"name":..., "kind":..., "api_base":..., "protocol":..., "auth_header":..., "auth_scheme":..., "upstream_api_key":..., "credential_kind":...}`. `kind` moves an endpoint between `static`/`cloud` and `dynamic` — the only way to hand one a human typed in over to the agent on its host, since registration deliberately never converts a static provider into one that can expire. Leaving `dynamic` clears the lease and any degradation with it, because the sweep reads `lease_expires_at` whatever the kind and an expired one would report the provider unreachable for ever. Becoming `dynamic` leaves the lease NULL, which the sweep reads as "not lapsed", so it is probed on its merits until its agent beats. Rotation is the other reason this exists: one credential serves every model on a provider, so replacing it is one write rather than one per model. An absent `upstream_api_key` leaves the stored one alone; `""` clears it. An absent field is left alone throughout |
| `GET /admin/provider-catalogue`              | Known providers and how to reach them: base URL, protocol, auth header, and the `credential_kinds` each accepts — only Vertex AI has more than `static`, so the UI asks that question only where there is an answer to give. Covers the entries `docs/providers.md` documents an *endpoint* for — it names about a hundred providers and gives a host for thirty-odd, and seeding the rest would mean inventing base URLs. A convenience, never a limit: anything speaking the OpenAI API works whether or not it is listed |
| `POST /admin/providers/register`             | `{"api_base":..., "node":..., "name":..., "engine":..., "ttl_seconds":...}`. `name` is the agent's: it is the thing that knows what the endpoint *is*, where the control plane only ever saw an address. Sent on every heartbeat, so changing it on the agent renames the provider — safe because routing resolves a target by its **model's** name, never its provider's. A name another provider already holds is declined with a warning and the heartbeat still succeeds, since a collision is no reason to let a lease lapse. Registers or refreshes a dynamic provider's lease. Needs a token and nothing more — registering is not an exposure, since a learned model reaches nobody until a frontend model points at it. Never converts a provider a human configured into one that can expire |
| `GET /admin/providers/{id}/available-models` | What that provider is serving right now, from the same `GET /v1/models` the sweep uses, with the already-registered ones marked. Registers nothing: OpenRouter answers with hundreds. `502` if the provider does not answer |
| `DELETE /admin/providers/{id}`               | Removes a provider that serves no models. **409 while any remain**, naming them — a cascade would take frontend model targets and usage history with it |
| `GET /admin/prompt-classes`                  | Classes, example counts, and whether each is _routable_ (has a centroid)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `POST /admin/prompt-classes`                 | `{"name":..., "tier":"fast"\|"refined", "min_margin":..., "refines":[...], "examples":[...]}`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `POST /admin/prompt-classes/{id}/examples`   | Add one example prompt                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `DELETE /admin/prompt-classes/{id}`          | Cascades to its examples and refinements                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `POST /admin/prompt-classes/evaluate`        | Per-class precision, recall, margins, nearest neighbours and a verdict — leave-one-out over your own examples                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `GET /admin/fallback-model`                  | The model every routing chain falls back to                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `PUT /admin/fallback-model`                  | `{"model_id": 42}` to set it, `{"model_id": null}` to clear it                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `PATCH /admin/principals/{id}`               | `{"name":..., "email":...}`. Safe to rename: keys, roles, limits, budgets and usage all reference a principal by id. `usage_events` records the name rather than referencing it, and is left alone — it says who a request was billed to at the time it was served |
| `PATCH /admin/roles/{name}`                  | `{"name":..., "description":...}`. A role's grants and its holders reference it by id, so renaming changes only the URL it is addressed at. A role carries no grant of its own — `permissions.resource` names models, servers and agents, never roles |
| `GET /admin/roles`                           | Roles and the permissions each one grants                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `GET /admin/limits`                          | Every principal with a configured rate limit                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `PUT /admin/principals/{id}/limits`          | `{"requests_per_min":..., "tokens_per_min":...}`. Either or both; upserts the one row this principal may have                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `DELETE /admin/principals/{id}/limits`       | Remove the limit — the principal becomes unlimited, not limited to zero                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `GET /admin/budgets`                         | Every principal with a configured token budget, including current consumption                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `PUT /admin/principals/{id}/budget`          | `{"tokens_total":..., "window":"daily"\|"weekly"\|"monthly"}`. Upserts the one row this principal may have; leaves `tokens_used` and the window's start alone on an update                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `DELETE /admin/principals/{id}/budget`       | Remove the budget — the principal becomes unlimited, not limited to zero                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `PATCH /admin/provider-models/{id}`                   | Correct a model in place: `{"description":..., "input_price_per_mtok":..., "output_price_per_mtok":..., "cache_ttl_seconds":..., "context_length":...}`. Every field optional; an explicit `null` clears, an absent field is left alone. `context_length` must be positive — a model that accepts no tokens is not a thing, so 0 is refused rather than read as "undeclared"                                                                                                                                                                                                                                                                  |
| `POST /admin/roles`                          | `{"name":..., "description":...}`. Permissions attach to roles, so a role is the only place to express "this caller may reach these models and nothing else"                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `DELETE /admin/roles/{name}`                 | Refused while any principal still holds it — a cascade would take every holder's access away at once, and the symptom arrives long after the click                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `POST /admin/roles/{name}/permissions`       | `{"verb":"model:invoke", "resource":"model/gpt-4o"}`. The verb list is closed — a permission nothing checks would read on a matrix as though it granted something                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `DELETE /admin/roles/{name}/permissions`     | Same body; revoke one                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `GET /admin/audit`                           | The change log, newest first. `?limit=&before=&actor_id=&target=&since=`. `before` is keyset pagination on the id of the oldest row you hold — an offset would skip or repeat rows as new ones arrive at the head                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `GET /admin/usage`                           | Aggregate requests, tokens, latency and spend. `?group_by=model\|principal\|virtual_model\|day&since=&until=&limit=`. `virtual_model` groups on what the caller _asked for_, which is the only grouping that can answer "how much traffic does each frontend model carry" — by the time a model is chosen the virtual name is gone. Reports `unpriced_requests` alongside every total: a request whose model has no price contributes nothing to `cost`, and summing those as zero would understate spend silently                                                                                                                             |
| `GET /admin/timeseries`                      | The same facts bucketed over time, for charts. `?since=&until=&bucket=<seconds>&model=&principal_id=`. Every bucket in the range is returned, **including empty ones as explicit zeros** — an aggregate that omits them makes a chart draw a straight line across an outage. Latency percentiles are the exception and come back `null` for an empty bucket, because zero would read as "instantaneous" rather than "nothing to measure". `bucket` is a floor, not an instruction: a width finer than the range can afford is widened so the series never exceeds 720 points, and the width actually used is implied by the returned instants |
| `GET /admin/fleet`                           | What each proxy replica can see — its backends' health, in-flight counts, and the snapshot version it is serving                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `POST /admin/routing/dry-run`                | `{"model":..., "streaming":..., "principal_id":..., "class":..., "headers":{...}}` → the candidate chain and **which rule index decided**                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `POST /admin/prices/sync`                    | `{"source":"open-router"\|"catalogue"\|"both", "overwrite":..., "dry_run":...}`. The same work `fastllm-proxy sync-prices` does, from a UI                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `GET /admin/config`                          | What this process was started with — role, TLS, poll and report intervals, cache bounds, session TTL, classifier tiers, OTLP. Read-only: changing one of these is a deploy                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `POST /admin/snapshot/rebuild`               | Rebuild and republish now. Answers with the version it published, because `refresh` deliberately does not fail the request that triggered it                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `POST /admin/sessions/revoke-all`            | Delete every session, including the caller's                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |

**No route returns a credential.** Key plaintext is shown once, by `POST /admin/keys`, and never again; `api_keys.hash` is a verifier, not a display value, and is not in any response. `upstream_api_key` is the one secret that cannot be reduced to a hash — the proxy has to present it upstream — so it is encrypted at rest and `GET /admin/provider-models` and `GET /admin/providers` report only whether one is set.

## Audit log

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

## Live backend health

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

Nothing is persisted. Health is a statement about _now_; a row saying a backend
was up two hours ago is history, not health. A replica that stops reporting
ages out after 30 seconds rather than lingering as "up, 40 minutes ago".

## Cost

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
curl -X PATCH .../admin/provider-models/42 -d '{"input_price_per_mtok":4000000}'
```

Absent means "leave alone" and an explicit `null` clears — so correcting a
price does not silently turn caching off, and a model can become unpriced
again. `GET /admin/provider-models` returns both prices and the cache TTL.

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
_token_. The data plane never learns this backend is different — it presents a
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
routing rules and frontend models all work the same against a translated
backend, and usage is reported from the provider's own token counts.
