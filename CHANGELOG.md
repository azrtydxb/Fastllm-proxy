# Changelog

Notable changes, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Nothing has been released yet.** There are no tags and `Cargo.toml` still says
`0.1.0`, so everything below is unreleased and the section is grouped by
capability rather than by version — inventing version numbers after the fact
would say something about stability that is not true. The first tagged release
starts a normal `## [x.y.z]` history.

This file was backfilled from `git log`; commit bodies carry the reasoning and
the measurements, and are the better source for *why* any of it is the way it is.

## [Unreleased]

### Gateway and request path

- OpenAI-compatible gateway over any number of backends, with responses
  forwarded byte-for-byte: an `openai` backend's body is never deserialised,
  re-encoded or buffered.
- Twelve proxied `POST` endpoints: `/chat/completions`, `/completions`,
  `/responses`, `/embeddings`, `/rerank`, `/score`,
  `/audio/{transcriptions,translations,speech}`, `/images/{generations,edits}`
  and `/moderations`.
- Cache-affinity routing with a load escape hatch — a shared prefix returns to
  the node holding its KV cache unless that node is meaningfully hotter than the
  least-loaded one. `least-loaded` and `round-robin` are selectable alternatives.
- The request path performs no I/O, enforced by `tests/no_io_on_hot_path.rs`.
- Owned upstream connections rather than a pooled client, after the pooled one
  was measured as the cause of a 6× throughput difference.
- Graceful shutdown: SIGTERM stops accepting, lets in-flight generations finish
  up to `--shutdown-grace` (25s), and logs anything still open when it expires.

### Routing

- Virtual models: ordered rules, weighted and ordered targets, and a failover
  chain across *models*, not just replicas.
- Rule conditions on principal, role, prompt and generation length, streaming,
  request headers, budget consumption, per-backend in-flight count, and time of
  day with weekday and UTC offset.
- Two-tier semantic routing — a ~115 µs static-embedding tier and an optional
  int8 ONNX transformer that only loads when a rule names a refined class.
- `POST /admin/routing/dry-run` answers which rule decided and what the chain
  resolved to, without dispatching anything.
- A deployment-wide fallback model appended to every chain, authorised like any
  other candidate so it can never widen a caller's reach.

### Providers and protocols

- 42 providers reachable as configuration; 40 speak the OpenAI API, 2 are
  translated.
- Native Anthropic (Messages) and Gemini (`generateContent`) translation in both
  directions, including streaming, tool calls, and image and audio inputs.
- Per-backend `protocol`, `auth_header`, `auth_scheme` and `default_max_tokens`
  — reachable from both the control plane and a YAML file, which is what makes
  Azure OpenAI (`api-key` with no `Bearer` prefix) and native backends
  configurable without the database.
- GCP service-account credentials minted and refreshed for Vertex AI.

### Control plane, RBAC and accounting

- Control plane / data plane split behind `--role`, sharing a pre-flattened
  snapshot; `AppState::apply_snapshot` is the single write path.
- RBAC with real API keys: principals, roles, permissions, per-model
  `model:invoke` grants. Keys hashed with SHA-256, passwords with Argon2id.
- Upstream credentials encrypted at rest with AES-256-GCM.
- Per-principal rate limits with cross-replica reconciliation, token and spend
  budgets over fixed windows, and `x-ratelimit-*` response headers.
- Usage accounting folded from a bounded tail buffer parsed once at end of
  stream; costs in integer micro-units, with prices synced from published
  catalogues and a provider-reported cost taking precedence.
- Append-only audit log recorded by a layer over every mutating route, with
  keyset pagination.
- Exact-match response cache, opt-in per model, bounded by both entries and
  bytes and dropped whole on any snapshot change.

### Operations

- Embedded React admin UI — thirteen screens covering fleet, backends, models,
  routing, classes, keys, RBAC, limits, usage, audit and settings.
- Per-replica health reports over the existing proxy-token channel, surfaced by
  `GET /admin/fleet`; kept per replica and never merged, so a partition and a
  dead backend stay distinguishable.
- Prometheus metrics including latency histograms, cache counters and the
  snapshot version, plus optional OTLP tracing behind the `otel` feature.
- Reload in place: SIGHUP or a snapshot poll swaps the routing table atomically
  without disturbing in-flight generations.
- Runs as one binary in three shapes (`all`, `control`, `proxy`); Kubernetes
  manifests in `deploy/`.

### Testing

- `tests/protocol_fuzz.rs` — mutation fuzzing over the Anthropic and Gemini
  translators, asserting no input panics, including arbitrary SSE chunk
  boundaries.
- `tests/doc_claims.rs` — countable claims in the README checked against the
  tables they count.
- `web/test/` — every screen mounted against wire-format fixtures, every control
  clicked, and the request each mutation sends asserted against the handler that
  receives it.
- Benchmarks against LiteLLM, with the conditions and the unfavourable results
  recorded alongside the favourable ones in `docs/performance.md`.
