# TODO

## P0: control plane and RBAC — done

Real per-key RBAC (principals, roles, permissions, SHA-256-hashed API keys),
a control/data-plane split (`--role=control`/`proxy`, sharing `Snapshot` as
the sole contract), three snapshot sources (file, http-poll, in-process),
`fastllm-proxy import` to migrate a `File`-mode deployment onto a database,
and the CI/deployment/docs wiring in this task. Design and self-review:
`docs/superpowers/specs/2026-08-06-control-plane-rbac-routing-design.md`.

Known gaps carried forward rather than silently fixed:

- ~~**The admin API has no authentication of its own.**~~ Fixed in P4:
  `/admin/*` now requires a session cookie (`POST /login`, Argon2id against
  `principals.password_hash`; see `src/control/auth.rs` and README.md's
  "Admin authentication" section). `/snapshot`, `/usage` and
  `/limits/reconcile` remain gated on the proxy token, deliberately — those
  are proxy processes authenticating to the control plane, not humans with
  passwords. `/admin` should still never be exposed outside the cluster: a
  login screen on a public listener is still the wrong default. See
  `deploy/README.md`.
- ~~`model_backends.upstream_api_key` is stored unencrypted at rest.~~ Fixed:
  `src/control/secrets.rs` encrypts it with AES-256-GCM before
  `import`/the admin API write it, `build_snapshot` decrypts it back on read,
  and `--role control`/`all` refuse to start without `FASTLLM_ENCRYPTION_KEY`
  rather than falling back to plaintext (`migrations/0004_encrypted_upstream_api_key.sql`).
  This protects the database, not `/snapshot` — the proxy still receives the
  credential in usable plaintext form, because it has to present it to the
  backend. `/snapshot` must still be TLS wherever a backend has a real
  credential. See README.md's "Encryption at rest" section.

All later phases are now implemented too: virtual models and rule-based
routing (P1), rate limits with reconciliation (P2), usage accounting and
budgets (P3), and the embedded management UI with session authentication
(P4). `POST /usage` is wired end to end — the data plane's tail buffer reads
the upstream's real token counts and the control plane folds them into
`budgets.tokens_used`.

## Features

### Embedded management and monitoring UI — done (P4)

Implemented: a small React dashboard (`web/`), embedded into the binary and
served by `--role=control`/`all` only (`src/control/ui.rs`), gated behind the
session-cookie login described in README.md's "Admin authentication"
section. The rest of this section is the original design note, left as-is
for the reasoning behind the choices it made (`rust-embed` over
`include_dir!`, the Dockerfile `node` stage over a `build.rs`) — those
tradeoffs are exactly what got built.

Serve a small React dashboard from the binary itself, the way Go's `embed.FS`
is normally used.

`rust-embed` is the direct analogue: `#[derive(RustEmbed)] #[folder = "web/dist/"]`
reads from disk in debug builds (so the frontend hot-reloads during
development) and bakes the bytes into the binary in release. `include_dir!` is
the lighter option if the runtime crate is unwanted; `include_bytes!` covers
single files.

Two reasons this fits here specifically:

- It lands on routes the proxy already serves locally (`/`, `/ui/*`), so it is
  completely off the byte-pump path. No effect on the hot path.
- It keeps the single-binary, single-container deployment — no sidecar, no
  second Service, nothing new to deploy alongside it on kw.

`/health` and `/metrics` already expose everything a dashboard needs: per-backend
in-flight, request and error totals, health state, active policy, uptime and the
model list. The UI is a rendering job, not a new API — though a small
`/v1/stats`-style endpoint with a time series would beat polling `/metrics` and
diffing counters client-side.

Two real costs, neither hidden:

1. **Build ordering.** `cargo build` fails outright if `web/dist/` is missing.
   Either a `build.rs` that shells out to npm — which makes cargo depend on node
   for everyone, including CI jobs that only want to run tests — or an extra
   `node` stage in the existing multi-stage Dockerfile that builds the SPA and
   copies `dist` into the Rust stage. The Dockerfile route is preferable: CI
   already has the shape for it and `cargo test` stays node-free.
2. **Caching.** `ETag` and `Cache-Control` have to be hand-rolled. rust-embed
   exposes each file's hash so it is a few lines, but it is not free the way a
   real static server is.

Also worth deciding before building it: whether the dashboard sits behind the
master key. `/health` and `/metrics` are deliberately open so probes and
Prometheus work without a key, and both already leak backend addresses — a UI
on the same terms is consistent, but it is a more inviting target on a VIP.

## Performance

Backlog with the measurements that justify (or kill) each item.

Numbers come from `cargo build --release` on a 10-core arm64 macOS host, driven
by a mock SSE upstream that flushes every frame with no think time — the worst
case for framing overhead. A real vLLM batches tokens, so production numbers
should be better than these. Re-measure before acting on any of this.

### Context: where the time goes today

- **Streaming cost is per-frame, not per-byte.** Same bytes, same event count,
  only the framing changed: 500 single-event frames → 77 MiB/s; 5 hundred-event
  frames → 2918 MiB/s. The byte pump is fine at ~3 GB/s. The ceiling is roughly
  **650k frames/s** regardless of frame size.
- **That ceiling was the upstream client, not this code — now fixed.** The
  pooled client cost one cross-task wakeup per frame. Replacing it with a
  connection this process owns and drives from inside the response body took
  streaming from 1314 to 7921 req/s and 78 to 471 MiB/s, a little over 6x, with
  no change to non-streaming. See `src/upstream.rs`.
- **The request path is ~2% of a request.** All of this proxy's own per-request
  work measures ~0.76µs (URL format + `Uri` parse 156ns, bearer header 92ns,
  header copy 97ns, `BodyPeek` parse 229ns at 1KiB, prefix hash 108ns, path
  allocations 41ns) against ~38µs of core time per request. The rest is kernel,
  socket and hyper protocol work.

**Measured against the real spark2 replica (2026-08-06).** One stream: TTFT
90ms proxied vs 93ms direct, inter-token 27.54ms vs 27.47ms. Eight concurrent
streams: 121.8 tok/s aggregate proxied vs 118.7 direct, inter-token 63.4ms vs
62.2ms. The proxy is not measurable in either. Crucially, vLLM emits **one SSE
event per HTTP frame, tens of milliseconds apart** — so there is nothing to
coalesce there either, and the per-frame ceiling is four orders of magnitude
from being reached.

### Worth doing

Nothing identified. The three items that used to live here are all resolved
below — one implemented, two measured and closed.

### Measured and rejected — do not retry without new evidence

- **Byte-level relay after the response is committed.** A dumb bidirectional
  TCP relay — parsing nothing, framing nothing, the hard ceiling for any proxy
  on this path — does 694 MiB/s where this proxy does 524 MiB/s and a single
  direct hop does 951 MiB/s. So the whole prize is **1.32x**, and only if
  detecting the end of a response were free. It is not: the in-flight guard has
  to be released, the connection returned to the pool, and the next request
  served on the client socket, all of which need the framing this would skip.
  Paying for that with a hijacked client socket and no pooling is a bad trade.
- **Coalescing already-arrived frames.** Merge ratio measured at exactly 1.000
  in three separate settings: against the pooled client, against the owned
  connection that replaced it, and against a real vLLM. There is never a second
  frame waiting. Confirmed dead; it was the deleted wakeup, not batching, that
  gave the 6x.
- **Hand-rolled `model` scanner to skip the JSON parse.** 67.1k → 67.2k req/s
  on 64KiB bodies. `serde_json` skips what it does not want at ~16 B/ns and the
  parse is ~3% of a request; a bespoke parser on the routing path is not worth
  the risk of misrouting.
- **Pre-parsed `Uri` per backend per endpoint.** ~0.2%, and it forces an
  endpoint-index coupling between `proxy.rs` and `registry.rs`.
- **Anything else on the request path.** There is under 1µs available in total.

## Multi-provider support

Done (2026-08-07):

- Every OpenAI-compatible provider — OpenRouter, Groq, DeepSeek, xAI, Together,
  Fireworks and the rest — reachable with no code, as a backend row. Base URLs
  are in `README.md`'s provider table.
- Per-backend `auth_header`/`auth_scheme`, so a provider that wants a raw key
  in `x-goog-api-key` works. Previously `Bearer` in `authorization` was
  hardcoded into `Backend::new`.
- Native Anthropic and Gemini translation behind `backends.protocol`, opt-in
  per backend, with byte-exact passthrough preserved and pinned by
  `tests/native_protocols.rs`.
- Health probes now authenticate. They sent no headers at all, so any keyed
  provider would have been permanently unhealthy.
- Backend identity now covers the whole configuration, not just
  `(api_base, upstream_model)` — rotating an upstream key used to leave the
  old credential in service until the process restarted, because live backend
  objects are carried across reloads to preserve in-flight counts.

Deliberately not done, each additive and small:

- Tool calling through a translated backend. Refused with `501` naming the
  feature; the work is a `tools` ⇄ `tool_use`/`functionDeclarations` mapping
  plus tool-call deltas in the stream re-framer. Reaching Claude *with* tool
  calling works today through OpenRouter, which is why this is not urgent.
- Multimodal (image/audio parts) through a translated backend — same shape of
  work, same `501` today.
- Cohere, Bedrock and Vertex. Bedrock needs SigV4 request signing and Vertex
  needs OAuth2 service-account tokens with background refresh; both put
  credential machinery near the request path and neither is reachable by
  configuration alone the way the other 21 providers are.

## Routing rules (2026-08-07)

Done:

- **Cross-model failover.** A rule's targets are an ordered chain, tried on
  5xx, 429 or an unreachable upstream. 429 is newly retryable — it is what a
  hosted provider's free tier returns, and health checks cannot see it because
  the pool is healthy and merely refusing. Ungranted candidates are filtered
  out, so failover never widens reach.
- **Header conditions** (`headers`), the cheapest useful knob: the client
  labels its own workload.
- **Streaming condition** (`stream`), a good proxy for "is a human waiting".
- **Budget conditions** (`min/max_budget_used_percent`), to degrade instead of
  refusing at the 402 cliff.
- **Capacity spill** (`max_inflight_per_backend`), expressed as a ceiling on a
  rule's own targets so spilling is ordinary first-match-wins ordering rather
  than a second mechanism.
- **Time windows** (`after`/`before`/`days`/`utc_offset_minutes`), with
  midnight wrap-around.
- Write-time validation, so a malformed condition is a 400 rather than a rule
  that silently never matches.

No migration was needed: conditions live in `routing_rules.match_json` (JSONB)
and targets were already an ordered weighted list.

Rejected, with reasons:

- **Regex/content matching on the prompt.** Scanning user text on the request
  path, easy to get subtly wrong, and it invites a policy engine to grow inside
  a gateway.
- **Semantic routing** (embed the prompt, route by similarity) is the idea
  worth having instead, and it needs its own design: an embedding per request
  is a network call on the hot path, which is the one thing this architecture
  forbids. Any viable version has to answer where the embedding comes from
  (in-process model? cached by prefix? computed only for a sampled subset?)
  before it is worth building.

## Semantic routing (2026-08-07)

Shipped. A prompt class is a name plus example prompts; the control plane
averages them into a centroid that ships in the snapshot, and the request path
takes the nearest one. No training step, no corpus, no model to fine-tune —
adding a class is a snapshot rebuild like adding a backend.

Two tiers, gated on configuration rather than on a flag. The fast tier
(potion-code-16M, a static token-vector lookup) runs at ~115us and separates
subject matter. The refined tier (bge-small, a transformer) runs at ~3.3ms and
separates classes that share a subject but differ in intent. If no routing rule
names a class that refines a fast-tier one, the transformer is never loaded and
no request can pay for it — `Classifier::escalate_from` is empty, and the fast
path is one atomic load and a length check.

Behind two cargo features: `classifier` pulls model2vec-rs, `classifier-tier2`
additionally pulls ONNX Runtime. The default build carries neither.

Measured, not assumed — every number is in docs/classifier.md, with the
benchmark harness in bench/. The findings that shaped it: classify by subject
rather than by verb (task-shaped classes fail on both tiers); class count is
not the problem, class definition is; and margins are not comparable across
models, so confidence floors are per class *and* per tier.

Not done, and recorded so it is not mistaken for done:

- `POST /admin/prompt-classes/evaluate` reports centroid collisions but not
  leave-one-out precision and recall over the operator's own examples. That is
  the diagnostic that turns "how many classes should I have" from an opinion
  into a measurement.
- Refined-tier weights are not baked into the image (130MB for a feature most
  deployments will not enable). Mount them and set --classifier-tier2-model.
- No classification cache. The prefix hash the router already computes would
  key one, so a multi-turn conversation classifies once instead of per turn.
  Worth doing only if 115us ever shows up in a profile, which it has not.
- Routing on *difficulty* remains out of scope: the 96% GSM8K-versus-lookup
  separation most likely measures genre rather than difficulty.
