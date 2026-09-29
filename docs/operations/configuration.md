# Roles and configuration

What each `--role` does, how a `File`-mode deployment moves onto a database,
and the config file's tuning knobs.

## Roles

One binary, three ways to run it, via `--role` (`FASTLLM_ROLE`):

| Role              | What it does                                                                                                               | Needs                                                                              |
| ----------------- | -------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| `proxy` (default) | Forwarding only, against either a control plane (`Http` mode) or a config file (`File` mode)                               | `--control-url` + `--proxy-token` (`Http` mode), or `--config` alone (`File` mode) |
| `all`             | Control plane and forwarding in one process, sharing state directly — no HTTP round trip between them                      | `--database-url`, `FASTLLM_ENCRYPTION_KEY`                                         |
| `control`         | Database, admin API (`/admin/*` — keys, principals, roles, models, backends), `/snapshot` and `/usage` — no proxy listener | `--database-url`, `FASTLLM_ENCRYPTION_KEY`                                         |

`proxy` is the default deliberately, not `all`: it is the only role that asks for nothing beyond what a pre-control-plane deployment already passed (`--config` and nothing else), so an existing deployment upgrades to this binary without gaining a new required flag. `all` and `control` are explicit opt-ins via `--role`/`FASTLLM_ROLE`.

`Http` mode degrades gracefully: a `proxy` that cannot reach its control plane at startup, or loses it later, falls back to the last snapshot it wrote to `--snapshot-cache` (default `/var/lib/fastllm/snapshot.json`) rather than refusing to start or dropping traffic.

### Migrating a `File`-mode deployment onto a database

```bash
fastllm-proxy import --config litellm_config.yaml --database-url postgres://...
```

Idempotent — seeds `providers`/`provider_models` **and the `auth:` block** (a `service_account` principal per key, the key itself as a SHA-256 hash, and its model grants) from a LiteLLM-format config, and can be run more than once safely.

Everything a backend row can hold is carried across, not just the address:

| from the file        | into `providers`/`provider_models`                                                                                                                                                                                                                                 |
| -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `api_base`           | the address, trailing slash trimmed                                                                                                                                                                                                                   |
| `model`              | `upstream_model`. A transport prefix (`openai/`, `vllm/`, `openrouter/`) is stripped; a wire-format prefix (`anthropic/`, `gemini/`) only when the backend speaks that protocol, so an OpenRouter id like `anthropic/claude-sonnet-4` survives intact |
| `api_key`            | `upstream_api_key`, AES-256-GCM encrypted before it reaches Postgres. LiteLLM's `not-needed`/`none` placeholders are treated as absent                                                                                                                |
| `protocol`           | `openai` (default), `anthropic` or `gemini`                                                                                                                                                                                                           |
| `auth_header`        | defaults to `authorization`; Azure OpenAI wants `api-key`                                                                                                                                                                                             |
| `auth_scheme`        | defaults to `Bearer`; `""` stores as NULL and sends the key raw                                                                                                                                                                                       |
| `default_max_tokens` | required in practice by an Anthropic backend                                                                                                                                                                                                          |

Two entries sharing a `model_name` become one model with two backends — a
load-balanced pool.

The `auth:` block carries its enforcement, not only its identity:

| from the file | into the database                                                                              |
| ------------- | ---------------------------------------------------------------------------------------------- |
| `key`         | `api_keys.hash` (SHA-256) plus a display prefix. Never stored in plaintext, never printed back |
| `name`        | a `service_account` principal, and a role `import:<name>` holding just that key's grants       |
| `provider_models`      | one `model:invoke` grant per model; `['*']` becomes allow-all                                  |
| `expires_at`  | the key's expiry                                                                               |
| `limits`      | the `limits` row — `requests_per_min`, `tokens_per_min`                                        |
| `budget`      | the `budgets` row, as a `monthly` window, because the file format has no window to carry       |

`budget.tokens_used` is written when the row is created and never on a
re-import. Once a budget is in the database it advances from real usage, and
letting a static number in a config file rewind it would hand back spend that
was already consumed.

**Re-importing an edited file converges.** A backend is keyed on
`(model, api_base, upstream_model)`: a row that already exists is updated
rather than duplicated, so a `protocol:` corrected in the file reaches the
database. The one exception is the credential, which is written only when the
file names one — a file with no `api_key` usually means the credential was set
through the admin API afterwards, and overwriting it with nothing on the next
import would revoke a working backend for no reason. Point `--role=all`/`control` at the same database afterward and the same keys keep working, with the same per-model authorisation they had in `File` mode.

Each imported key gets its own role, `import:<name>`, holding just that key's grants — `models: ['*']` becomes `model:invoke` on `model/*` (i.e. allow-all), a named list becomes one grant per model. Re-importing an edited file converges: grants dropped from the file are revoked, not merely left behind. `import` never prints a key back; the config file is the only copy of the plaintext.

Day-to-day changes after the initial seed go through the admin API below rather than another `import` run or hand-written SQL, so they reach a running control plane immediately instead of on its next periodic rebuild.

### First key, by API

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

(`import`, run on the host rather than inside the container, needs the same
`FASTLLM_ENCRYPTION_KEY` the control plane was given — they share one
database, so they must agree on one key.)

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
  prefix_bytes: 2048 # bytes of the raw body hashed for the affinity key
  balance_abs: 8 # absolute in-flight slack before affinity yields
  balance_rel: 1.5 # relative slack multiplier
  affinity_slots: 65536 # prefix-affinity cache entries
  unhealthy_after: 2 # consecutive failed probes before eviction
  consecutive_timeout_threshold: 2 # consecutive header timeouts before eviction
```

`openai/`, `vllm/`, `hosted_vllm/` and `openai_like/` prefixes are stripped from `litellm_params.model`; a name that is genuinely `Qwen/Qwen3-1.7B` keeps its org. `not-needed`, `none` and `null` API keys are treated as absent.

### Per-key RBAC in `File` mode

`--master-key`/`general_settings.master_key` is one shared secret for every client and is deprecated. The replacement in `File` mode (no `--control-url`) is an `auth:` block:

```yaml
auth:
  keys:
    - key: sk-...
      name: ci-pipeline
      models:
        ["qwen3-6-35b-a3b-nvfp4"] # `["*"]` for every model; an empty
        # or omitted list grants nothing
      expires_at: "2027-01-01T00:00:00Z" # RFC 3339, optional
      limits: # optional; absent means unlimited
        requests_per_min: 60
        tokens_per_min: 100000
      budget: # optional; absent means unlimited
        tokens_total: 1000000
        tokens_used:
          0 # optional starting point; static —
          # File mode has no reconciliation
          # loop to advance it on its own
```

Absent `auth:` means open (no key required) — today's behaviour when no master key is set either. In `Http` mode (`--control-url` given), `auth:` is ignored: keys live in the database and are managed through the control plane's admin API instead. `fastllm-proxy import` carries an existing `auth:` block into that database unchanged (see "Migrating a `File`-mode deployment" above), so the same keys authorise the same models on either side of the move. `limits` is `File` mode's mirror of the control plane's `limits` table (see "Rate limits" above) — either field alone, both, or neither. `budget` is the same mirror of the `budgets` table (see "P3: usage accounting and budgets" above).

### Protecting an engine from its own queue

vLLM accepts every request and queues the surplus itself, unbounded. A burst
does not fail -- it makes every request slow, including the ones already
running, until first-byte latency crosses the upstream timeout and the timeout
path ejects a backend that was working. Flow control moves that queue into the
proxy, where it is bounded and a caller can be told "not now" at once.

It is set **per backend**, on the Models page (the FLOW CONTROL cell of each
backend row) or with `PATCH /admin/backends/{id}` -- never in a config file,
because the right numbers are a property of the engine, and changing them must
not need a redeploy. `File` mode has no gates.

| Field | Default | Meaning |
|---|---|---|
| `admission_max_concurrent` | off (`null`) | Most concurrent requests each proxy replica sends the engine. Setting it turns the gate on; `null` turns it off. |
| `admission_high_water` | 4 | Engine queue depth (`vllm:num_requests_waiting`) at which the ceiling halves. |
| `admission_max_queued` | 64 | Requests allowed to wait at the proxy; beyond it they get 503. |
| `admission_max_wait_seconds` | 30 | How long a request waits for a slot before 503. |
| `upstream_timeout_seconds` | `--upstream-timeout` | Time to first byte before the request fails. Edited in the same dialog because it is the other half of the same problem. |

The engine scraper (`--engine-scrape-interval`, default 2s) halves the gate's
ceiling while the engine reports `high_water` or more requests waiting, and
raises it by one per scrape once that queue has drained to zero. Requests past
the ceiling wait at the proxy, up to `max_queued` of them for up to
`max_wait_seconds`; beyond that they get **503 with `Retry-After`** and
`"type": "backend_overloaded"`. That is deliberately not 429: it is a statement
about the backend, not the caller's quota. Changes reach running proxies on
their next snapshot poll.

- **A pool still balances.** A request that finds its engine's gate full moves
  to an untried sibling without waiting, and only queues when there is nowhere
  else to go.
- **Each replica has its own ceiling**, so N proxies admit up to N times
  `admission_max_concurrent` between them. The engine's queue depth reflects all
  of their traffic, so each backs off on what the others sent too.
- **Running requests are never cut off.** A lowered ceiling takes effect as
  slots come free.
- **An engine with no `/metrics`** keeps a fixed ceiling at
  `admission_max_concurrent` -- still a concurrency cap, just not an adaptive one.

Start with `admission_high_water` at a few requests -- a small queue is an
engine keeping its GPU busy, not one falling behind -- and
`admission_max_concurrent` at roughly the engine's `--max-num-seqs` divided by
the number of proxy replicas.

**Seeing it work.** The Fleet page shows, per backend, the engine's own
running/waiting counts and the gate's slots in use over its current ceiling
(amber while it is holding traffic back); hover for admitted and refused totals.
The Models page shows the same in each backend's FLOW CONTROL cell. For
dashboards, `/metrics` carries:

| Series | Type | Meaning |
|---|---|---|
| `fastllm_backend_engine_requests{state="running"\|"waiting"}` | gauge | What the engine itself reports, when fresh. |
| `fastllm_admission_max_concurrent` | gauge | The configured ceiling. |
| `fastllm_admission_capacity` | gauge | The current ceiling; below the configured one while the engine is behind. |
| `fastllm_admission_in_use` | gauge | Slots held by requests in progress. |
| `fastllm_admission_queued` | gauge | Requests waiting at the gate. |
| `fastllm_admission_admitted_total` | counter | Requests let through. |
| `fastllm_admission_refused_total{reason="full"\|"timed_out"}` | counter | Refused at once (queue full, or moved to a sibling) or after waiting. |

All are labelled `api_base` and `model`, per replica; sum across replicas for
the fleet.

### Tuning affinity

`balance_abs` / `balance_rel` set how much imbalance is tolerated before cache locality is given up. Higher values favour cache hits; lower values favour even load. The default (8 requests absolute, 1.5× relative) suits a small cluster of a few nodes with long shared system prompts. If your traffic has little prefix sharing, `--policy least-loaded` is the honest choice and skips the bookkeeping.

`--policy lowest-latency` is for a pool whose members are **not** equivalent — a fast GPU beside a slower one, or a local node beside a hosted provider. Least-loaded is misled there: a slow backend with one request queued looks emptier than a fast one with two, so it keeps being fed. This ranks by an exponentially weighted mean of recent whole-request latency instead, tie-broken by in-flight so equally fast backends still balance.

Three properties worth knowing before choosing it:

- **A backend with no completed requests is eligible, not fastest.** Treating an unmeasured backend as 0 µs would hand it the whole pool before it proved anything; excluding it would mean a newly added backend never got a request and so never earned an estimate.
- **Backends within 12.5% of the best are treated as equal.** Without that band the pool oscillates — whichever backend last finished quickest wins every subsequent pick until its own queue slows it down.
- **It is cache-blind.** On matched nodes serving long shared prefixes, `cache-affinity` wins: a prefix cache hit is worth far more than a few hundred microseconds of measured difference between identical machines. That is why the default did not change.
