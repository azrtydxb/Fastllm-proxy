# fastllm-proxy

A low-latency OpenAI-compatible gateway for multi-node LLM serving, written in Rust.

It fronts several inference backends (vLLM, SGLang, llama.cpp — anything speaking the OpenAI API) behind one endpoint, and it is built for the case where a general-purpose gateway costs you throughput instead of adding it.

It reads LiteLLM-format config files unchanged, so it drops into an existing `sparkrun proxy` setup without rewriting anything.

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

`control` and `proxy` split for a cluster deployment where the admin API — which has **no authentication of its own yet** (see below) — needs to stay off the public listener while the proxy scales out independently. See `deploy/` for the manifests that wire this up on Kubernetes.

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
| `/admin/*` | `--role all`/`control` only. **No authentication at all** — not even `--proxy-token`. See the table below and the warning under it |
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
| `GET /admin/keys` | Prefix, name, principal, expiry, disabled. **Never** the key or its hash |
| `POST /admin/keys` | `{"name":..., "principal_id":..., "expires_at":...}`. Returns the plaintext key once |
| `DELETE /admin/keys/{id}` | Revoke (sets `disabled`; the row stays for audit) |
| `GET /admin/models` | Models and their backends. Reports *whether* a backend has an upstream credential, never the credential |
| `POST /admin/models` | `{"name":..., "description":...}` |
| `DELETE /admin/models/{id}` | Cascades to that model's backends |
| `POST /admin/models/{id}/backends` | `{"api_base":..., "upstream_model":..., "upstream_api_key":...}`. The credential is encrypted before it reaches Postgres and cannot be read back |
| `DELETE /admin/backends/{id}` | Remove one backend from a pool |
| `GET /admin/roles` | Roles and the permissions each one grants |
| `GET /admin/limits` | Every principal with a configured rate limit |
| `PUT /admin/principals/{id}/limits` | `{"requests_per_min":..., "tokens_per_min":...}`. Either or both; upserts the one row this principal may have |
| `DELETE /admin/principals/{id}/limits` | Remove the limit — the principal becomes unlimited, not limited to zero |

**No route returns a credential.** Key plaintext is shown once, by `POST /admin/keys`, and never again; `api_keys.hash` is a verifier, not a display value, and is not in any response. `upstream_api_key` is the one secret that cannot be reduced to a hash — the proxy has to present it upstream — so it is encrypted at rest and `GET /admin/models` reports only whether one is set.

**`/admin/*` has no authentication of any kind.** None of the routes above check anything — no header, no token, no session — so anyone who can reach the admin port can mint a key, grant a role, or repoint a backend. `--proxy-token` gates `/snapshot` only; it is not, and never was, a check that `/admin/*` also performs (an earlier version of this README claimed otherwise). Sessions and passwords for real admin auth are specified but land later, with the management UI (see `TODO.md` and `docs/superpowers/specs/2026-08-06-control-plane-rbac-routing-design.md`). Until then, network isolation is the *only* control: **never** put the admin port on a network-reachable listener — bind it to a cluster-internal Service or localhost only. `deploy/control.yaml` does this with a ClusterIP Service kept off the LoadBalancer VIP; do not merge them, and do not treat `--proxy-token` as covering `/admin/*` when deciding what is safe to expose.

### Encryption at rest

`model_backends.upstream_api_key` is encrypted at rest with AES-256-GCM (`src/control/secrets.rs`; `ring::aead`, already in the dependency tree via rustls) before `import`/the admin API ever write it to Postgres, and decrypted by `build_snapshot` when the control plane builds a snapshot. This protects the **database**, not the **snapshot**: `/snapshot` still carries the credential in usable plaintext form, because the proxy has to present it to the backend as a bearer token — an upstream credential cannot be reduced to a hash the way `api_keys.hash` is. `/snapshot` must be TLS wherever a backend has a real credential, exactly as before this existed. What encryption at rest actually buys: someone with read access to Postgres (a backup, a replica, a leaked `pg_dump`) no longer gets every upstream credential for free.

`--role control`/`all` and `fastllm-proxy import`/`reencrypt-backends` all require `FASTLLM_ENCRYPTION_KEY` — 32 bytes, hex-encoded (e.g. `openssl rand -hex 32`) — and refuse to start without it rather than falling back to plaintext. `--role proxy` never touches the database and never requires it. A database that already has plaintext rows from before this existed (a developer's scratch database — the live cluster has no control-plane database deployed at all yet) needs the one-shot `fastllm-proxy reencrypt-backends --database-url <url>` command run once; see `migrations/0004_encrypted_upstream_api_key.sql` for why this is a command rather than a format the read path silently tolerates forever.

### TLS on `/snapshot` and `/usage`

`/snapshot` carries `model_backends.upstream_api_key` in usable plaintext form (see "Encryption at rest" above), so it — and `/usage`, gated by the same token and sharing the same listener — must be TLS in any deployment where a backend has a real credential.

`--role control`/`all` take `--tls-cert`/`--tls-key` (PEM, `FASTLLM_TLS_CERT`/`FASTLLM_TLS_KEY`). Give both and the admin API — `/admin/*`, `/snapshot`, `/usage`, all of it, since they share one listener — serves HTTPS via `rustls`/`tokio-rustls` (already dependencies; no new TLS crate). Give neither and it serves plain HTTP, logging a startup warning every time it does, because a dev deployment with no real backend credentials is legitimate and must not be forced to generate a cert it does not need — but the fallback must never be silent. Giving only one of the two is a startup error, not a silent fall-back to HTTP.

On the client side, `--role proxy` in `Http` mode (`--control-url https://...`) and any `https://` backend `api_base` both go through the one pooled `Upstream` client (`src/upstream.rs`), which already speaks TLS. `--ca-bundle` (`FASTLLM_CA_BUNDLE`) adds one or more PEM CA certificates to the trust store *in addition to* the system roots — required to trust a private or self-signed cert (a cert-manager-issued, in-cluster control-plane certificate is the normal case; see `deploy/control.yaml`'s `fastllm-control-tls` `Certificate` and `deploy/README.md`'s TLS section) that no public root store contains. Without it, `--control-url https://...` against such a cert fails the handshake.

### `POST /usage`: the reverse channel

Defined now so the wire protocol never has to reshape later, even though nothing calls it yet — P2 wires real token counting into the request path. `--role proxy` holds a `usage::UsageReporter`: a bounded queue plus a background flush task that batches events and posts them to `/usage` on its own schedule, authenticated with the same `--proxy-token` as `/snapshot`. Recording an event (`UsageReporter::record`) is a non-blocking `try_send` — a full queue means the control plane is not keeping up, and the event is dropped rather than applying backpressure to inference (the design's stated tradeoff: "dropping usage rather than blocking a request is deliberate — billing accuracy is not worth failing inference"). Dropped events are counted and exposed on `/metrics` as `fastllm_usage_reports_dropped_total`, so the loss is visible instead of silent.

On the control-plane side, `POST /usage` accepts a batch and persists it to `usage_events`. A record naming a `principal_id` or model that no longer exists is dropped from the batch rather than failing the whole request — one stale id from one replica must not poison every other principal's usage in the same flush interval.

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
```

Absent `auth:` means open (no key required) — today's behaviour when no master key is set either. In `Http` mode (`--control-url` given), `auth:` is ignored: keys live in the database and are managed through the control plane's admin API instead. `fastllm-proxy import` carries an existing `auth:` block into that database unchanged (see "Migrating a `File`-mode deployment" above), so the same keys authorise the same models on either side of the move. `limits` is `File` mode's mirror of the control plane's `limits` table (see "Rate limits" above) — either field alone, both, or neither.

### Tuning affinity

`balance_abs` / `balance_rel` set how much imbalance is tolerated before cache locality is given up. Higher values favour cache hits; lower values favour even load. The default (8 requests absolute, 1.5× relative) suits a small cluster of a few nodes with long shared system prompts. If your traffic has little prefix sharing, `--policy least-loaded` is the honest choice and skips the bookkeeping.

## Behaviour notes

- **Retries** only happen before any byte has been forwarded. Once the response is committed a mid-stream failure propagates as-is — it cannot be silently retried without corrupting the stream.
- **5xx is retried, 4xx is not.** A client error retried across every node is the same client error three times.
- **The last backend's response is forwarded verbatim.** A 5xx is only retried while another backend remains; when none does, the upstream's own status and body reach the client rather than a synthetic 502. On a single-node pool that means every error keeps the engine's diagnostics.
- **Audio endpoints take `multipart/form-data`.** `model` is read from the form field and the upload is forwarded byte for byte, content-type and boundary intact. An alias splices the new name into that one field rather than re-encoding the body.
- **`https://` backends work**, so a TLS-terminated or hosted endpoint can sit in the same config as cluster-local nodes. System root certificates are used, falling back to the bundled Mozilla set.
- **A backend that fails every probe is still used as a last resort** rather than returning 503. A stale health flag should not turn a recoverable request into an outage.
- **The client's `Authorization` header is never forwarded.** It authenticates the client to the proxy; the upstream gets the backend's own key or none.
- **Affinity keys hash the raw request prefix**, not parsed fields. JSON does not guarantee field order, but order is stable per client, which is all affinity needs — a client that reorders per request degrades to least-loaded rather than misrouting.
- **A rate-limited request gets `429` with `Retry-After`**, checked after authorisation and model resolution but before the request is dispatched upstream — nothing is forwarded on a rejected request. See "Rate limits" above.

## Development

```bash
cargo test
cargo build --release
```

## License

Apache-2.0
