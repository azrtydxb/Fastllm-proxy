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
| `all` (default) | Control plane and forwarding in one process, sharing state directly — no HTTP round trip between them | `--database-url`, or nothing for `File` mode with `--config` alone |
| `control` | Database, admin API (`POST /admin/keys`, `DELETE /admin/keys/{id}`) and `/snapshot` — no proxy listener | `--database-url` |
| `proxy` | Forwarding only, against either a control plane (`Http` mode) or a config file (`File` mode) | `--control-url` + `--proxy-token` (`Http` mode), or `--config` alone (`File` mode) |

`control` and `proxy` split for a cluster deployment where the admin API — which has **no authentication of its own yet** (see below) — needs to stay off the public listener while the proxy scales out independently. See `deploy/` for the manifests that wire this up on Kubernetes.

`Http` mode degrades gracefully: a `proxy` that cannot reach its control plane at startup, or loses it later, falls back to the last snapshot it wrote to `--snapshot-cache` (default `/var/lib/fastllm/snapshot.json`) rather than refusing to start or dropping traffic.

### Migrating a `File`-mode deployment onto a database

```bash
fastllm-proxy import --config litellm_config.yaml --database-url postgres://...
```

Idempotent — seeds `models`/`model_backends`/keys from a LiteLLM-format config and can be run more than once safely. Point `--role=all`/`control` at the same database afterward.

### Quickstart with docker-compose

`docker-compose.yml` in the repo root brings up Postgres and `--role=all` together:

```bash
docker compose up
# proxy on :4000, admin API on :4001, postgres on :5432
fastllm-proxy import --config litellm_config.yaml \
  --database-url postgres://fastllm:fastllm@localhost:5432/fastllm
curl -XPOST localhost:4001/admin/keys -H 'content-type: application/json' \
  -d '{"name":"local","principal_id":1}'
```

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
| `POST /admin/keys`, `DELETE /admin/keys/{id}` | `--role all`/`control` only. Bearer-token gated by `--proxy-token`, but that token is a placeholder, not real admin auth — see below |
| `GET /snapshot` | `--role all`/`control` only. What `--role proxy` polls in `Http` mode; gated by `--proxy-token` |

**The admin API has no authentication of its own yet.** `--proxy-token` is the only credential either `/admin/*` or `/snapshot` currently check, and it is shared with the proxy's own polling — anyone who has it can create keys, not just read the snapshot. Sessions and passwords for real per-admin auth are specified but land later, with the management UI (see `TODO.md` and `docs/superpowers/specs/2026-08-06-control-plane-rbac-routing-design.md`). Until then, **never** put `/admin` or `/snapshot` on a network-reachable listener — bind the admin port to a cluster-internal Service or localhost only. `deploy/control.yaml` does this with a ClusterIP Service kept off the LoadBalancer VIP; do not merge them.

**`model_backends.upstream_api_key` is stored unencrypted.** There is no encryption-at-rest layer in this codebase for that column (see `migrations/0002_correct_upstream_api_key_comment.sql`), so database read access is equivalent to upstream-credential access. Restrict who can read the control plane's Postgres accordingly.

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
      models: ["qwen3-6-35b-a3b-nvfp4"]   # or omit/`["*"]` for every model
      expires_at: "2027-01-01T00:00:00Z"  # RFC 3339, optional
```

Absent `auth:` means open (no key required) — today's behaviour when no master key is set either. In `Http` mode (`--control-url` given), `auth:` is ignored: keys live in the database and are managed through the control plane's admin API instead (`POST /admin/keys`, `DELETE /admin/keys/{id}`).

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

## Development

```bash
cargo test
cargo build --release
```

## License

Apache-2.0
