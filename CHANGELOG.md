# Changelog

Notable changes, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Commit bodies carry the reasoning and the measurements and remain the better
source for *why* anything is the way it is; this file is the summary.

## Unreleased

### Added

- **MCP gateway.** One endpoint in front of every tool server, with the same
  keys and the same grant machinery as models. A server is a row; a grant is
  `mcp:invoke` on `mcp/<name>` and is deliberately not implied by
  `model:invoke`, because tools have side effects and models do not. Tools
  arrive namespaced `<server>__<tool>` so two servers can both expose
  `search`. `GET /v1/mcp/servers`, `POST /v1/mcp/tools/list`,
  `POST /v1/mcp/tools/call`, an **MCP servers** screen, and admin CRUD under
  `/admin/mcp-servers`. `stdio` servers are deliberately unsupported — see
  docs/mcp.md.
- **A2A gateway.** One address in front of every agent: `GET /v1/agents`, the
  agent card at `/v1/agents/{name}/.well-known/…` **rewritten to point at this
  gateway** so the client's next call is still authorised and attributed, and
  `POST /v1/agents/{name}` carrying every JSON-RPC method. Protocol versions
  are pinned per agent rather than inferred, forwarded methods are a closed
  list, and `agent:invoke` is implied by neither `model:invoke` nor
  `mcp:invoke`. An **Agents** screen and `/admin/a2a-agents` CRUD. Translation
  between 0.3 and 1.0 is deliberately not done — see docs/agents.md.
- **Interactive API reference** on the docs site, rendering the same
  `openapi.json` the control plane serves at `/openapi.json`.

### Fixed

- `import` dropped four backend fields it had already parsed. `protocol`,
  `auth_header`, `auth_scheme` and `default_max_tokens` are declared in the
  config schema so a YAML file can describe an Anthropic or Azure backend, and
  `Registry::build` honours them — but `import` wrote three columns, so the
  same file produced an `openai` backend on `Bearer` auth once it reached the
  database. Nothing warned: every dropped field has a valid default. Anyone who
  imported a native-protocol backend should re-run `import`, which now
  converges an existing row instead of only avoiding a duplicate.
- The `FileSource` path dropped the same four, so one YAML file described a
  different backend depending on which code path read it.
- `auth_scheme` was two states where it needed three. Absent means
  `Authorization: Bearer <key>`; `""` means send the key with no prefix, which
  is what Azure's `api-key` and Anthropic's `x-api-key` require. Treating
  absent and empty alike stripped `Bearer` from every File-mode backend that
  did not mention the field — a request every OpenAI-compatible upstream
  rejects. Found by running an import against a real database and reading the
  row.
- `import` dropped the `limits:` and `budget:` blocks from `auth.keys`
  entirely, so a key imported out of a rate-limited `File`-mode deployment
  arrived in the database unlimited. The key worked, which is why nobody
  would have looked.
- A LiteLLM `anthropic/`-style prefix was never stripped, so
  `anthropic/claude-sonnet-4` reached Anthropic as a model by that name. It is
  now stripped **only when the backend speaks that protocol**: to OpenRouter
  the same string is the model id and stripping it would ask for a model that
  does not exist. `openrouter/` joins the transport prefixes, and exactly one
  prefix is ever removed, so `openrouter/anthropic/claude-sonnet-4` becomes
  the OpenRouter id `anthropic/claude-sonnet-4`.

## [0.1.0] — 2026-08-13

First tagged release. Everything below was built before it, so this entry is
a description of what 0.1.0 *is* rather than a diff against something
earlier — grouped by capability, because there is no previous version to
compare against.

Published as `ghcr.io/azrtydxb/fastllm-proxy:v0.1.0` (linux/arm64).

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

### Accounting, history and the UI

- Usage recorded for **every** attributable request, not only for principals
  under a budget or a token limit — the narrower rule meant a deployment that
  enforced nothing recorded nothing.
- Refusals the gateway makes itself (403/429/402, and the 502 for an
  unreachable chain) recorded and tagged by kind, so a total backend outage no
  longer writes zero rows and reads as a quiet period. Unattributable refusals
  (401, unknown model) counted per replica per minute instead of rowed, since
  401 is the one refusal a stranger can trigger at will.
- `GET /admin/timeseries` serves that history bucketed, with empty buckets as
  explicit zeros and null latency where there was nothing to measure.
- 90 days of per-request rows, then hourly rollups kept indefinitely. Rollups
  carry no percentiles, because percentiles do not merge.
- Charts on Overview and Metrics with a click-through drill-down: five ranges,
  pan through history, filter by model or principal.
- Model `context_length`, and a routing rule that demotes a model which cannot
  hold the prompt plus the requested generation. Undeclared is never treated
  as too small.
- `--policy lowest-latency` for pools whose members are not equivalent.
- `--webhook-url` for backend up/down and snapshot-rebuild failure, HMAC-signed.
- `/v1/models` filtered to what the calling key may actually invoke.

### Documentation and packaging

- `openapi.json`, served at `/openapi.json` with Swagger UI at `/docs`, checked
  against the router in both directions by `tests/openapi.rs`.
- A Helm chart for deployments that are not this cluster.
- Client integration guide (SDKs, five coding agents, four frameworks) and a
  troubleshooting page seeded from failures that actually happened.
- A Grafana dashboard and a signature-verifying webhook receiver in `examples/`.

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
