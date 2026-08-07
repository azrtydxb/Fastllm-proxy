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

- **Tool calling through a translated backend.** Refused today with a `501`
  naming the feature (`protocol::mod.rs`'s `check_supported`), and reaching
  Claude *with* tool calling already works through OpenRouter, which is why
  this is not urgent. What it needs, concretely:

  1. **Request, Anthropic.** `tools[].function.{name,description,parameters}`
     becomes `tools[].{name,description,input_schema}` — near enough a rename,
     since both take JSON Schema. `tool_choice` maps
     `auto`/`none`/`required`/`{function:{name}}` onto
     `{type:auto|any|tool,name}`, with `none` having no direct equivalent
     (drop the tools instead of sending an unsupported value).
  2. **Request, Gemini.** The same schema becomes
     `tools[].functionDeclarations[]`, and `tool_choice` becomes
     `toolConfig.functionCallingConfig.mode` = `AUTO`/`ANY`/`NONE`.
  3. **Conversation history.** This is the part that is not a rename. An
     OpenAI transcript carries `assistant.tool_calls[]` and separate
     `role:"tool"` messages keyed by `tool_call_id`. Anthropic carries
     `tool_use` and `tool_result` blocks *inside* user/assistant messages;
     Gemini uses `functionCall`/`functionResponse` parts. So
     `OpenAiRequest::split_system` — which today flattens every message to a
     string and refuses anything else — has to become a real message mapper
     that pairs each result back to its call. The `role:"tool"` arm that
     currently returns `Unsupported` is where this lands.
  4. **Non-streaming response.** Anthropic `content[]` gains `tool_use` blocks
     with `{id,name,input}`; those become `choices[0].message.tool_calls[]`
     with `function.arguments` **stringified**, and `finish_reason` becomes
     `tool_calls`. The `stop_reason` mapping already handles `tool_use`.
     Gemini's `functionCall` parts map the same way.
  5. **Streaming.** The real work. Anthropic streams a tool call as
     `content_block_start` with a `tool_use` block, then
     `input_json_delta` fragments carrying *partial JSON*, then
     `content_block_stop`. OpenAI expects
     `delta.tool_calls[{index,id,function:{name,arguments}}]` where
     `arguments` accumulates as a string. So `StreamTranslator` has to track
     the open block index and emit indexed deltas — the partial JSON passes
     through as a string and must **not** be parsed mid-flight, which suits the
     existing design.
  6. **Refusals shrink.** `check_supported` stops rejecting
     `tools`/`tool_choice`, and `split_system` stops rejecting `role:"tool"`.
     Everything else it refuses stays refused.

  Testable without a provider: `src/protocol/tests.rs` already asserts exact
  request bytes and replays SSE byte-by-byte, so the fixtures extend the same
  way. Budget it as one focused change per protocol rather than both at once —
  the streaming re-framer is where the bugs will be.
- Multimodal (image/audio parts) through a translated backend. Same shape,
  smaller: `content` parts of type `image_url` become Anthropic `image` blocks
  with base64 `source`, or Gemini `inline_data`. The refusal lives in
  `Content::into_text`, which flattens to a string today. No streaming
  complication — images are input only.
- Cohere, Bedrock and Vertex. Bedrock needs SigV4 request signing and Vertex
  needs OAuth2 service-account tokens with background refresh; both put
  credential machinery near the request path and neither is reachable by
  configuration alone the way the other 21 providers are.

## Test infrastructure: the Postgres connection ceiling — fixed (2026-08-07)

A full `--include-ignored` run exhausted the dev cluster's connections twice,
presenting as every end-to-end test failing at once with `PoolTimedOut` — which
reads as a code failure and is not. The cause was pool arithmetic nobody had
done: every process that talks to Postgres takes a whole *pool*, not a
connection, and `control::db::connect` hardcoded 8. Fourteen concurrent tests
each spawning a proxy is 112 against a limit of 100.

Three changes, because one alone would only move the ceiling:

- Pool size is now `FASTLLM_DATABASE_MAX_CONNECTIONS` (default 8), and every
  test that spawns a proxy sets it to 2. This is the fix that matters, and it
  is a real deployment concern too — several proxy replicas against one
  Postgres hit the same arithmetic.
- Pools release idle connections after 30s and cap connection lifetime at 30
  minutes. A process killed mid-test used to park its whole pool until the
  server timed the sockets out, which is how 86 idle connections accumulated.
- `max_connections` on the kw cluster raised from 100 to 300, with the memory
  to back it (limit 512Mi to 2Gi).

After a full suite run the database now settles back to 3 connections.

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
- **Semantic routing** (embed the prompt, route by similarity) was the idea
  worth having instead. It has since been built — see "Semantic routing" below.
  The objection recorded here was that an embedding per request would be a
  network call on the hot path; the answer turned out to be an in-process
  static embedding at ~115µs, which is neither a network call nor measurable.

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

`POST /admin/prompt-classes/evaluate` reports leave-one-out precision, recall,
mean and worst margin, nearest neighbours, the misclassified examples
themselves, and a verdict per class. Both classifier models are baked into the
image; nothing loads the refined one unless a rule names a refined class.

One bug found while finishing this, and worth recording because the shape
recurs: escalation with a *single* refined contender had no runner-up, so the
margin degenerated to a raw similarity score. A margin-shaped floor like 0.10
is met by almost any prompt's raw similarity to almost any centroid, so one
refined class would have silently captured every request the fast tier assigned
to the class it refines — the exact opposite of what a refinement is for.
Escalation now requires at least two contenders, which is also the
configuration the measurements were taken on (architecture *against* coding, a
binary question).

Two more bugs, both found by running it on the cluster rather than by any
test, and both worth recording for their shape:

- **A `--role proxy` never classified anything.** `AppState` is constructed
  with a snapshot already in hand, which bypasses `apply_snapshot` — the single
  write path — and so bypassed the classifier rebuild. A proxy serving an
  unchanged snapshot classified nothing at all, silently, forever. Every
  end-to-end test passed because `--role all` writes through the admin API,
  which triggers a rebuild immediately, so the shape that actually ships was
  the one shape never exercised. `AppState::prime_derived_views` now exists for
  exactly this, and is the third bug on this branch caused by something
  bypassing `apply_snapshot`.
- **Refining a class silently broke every rule on its parent.** Once
  `debugging` refined `coding`, a request the refined tier called `debugging`
  no longer matched a rule saying `{"class": "coding"}`. Refinement is a
  *sub*-classification, so a refined answer now carries what it refines and
  satisfies rules on either; a more specific rule placed earlier separates them.

Verified live on kw with both tiers: a coding prompt escalates to the refined
tier, comes back `debugging`, matches the `coding` rule through the refinement
relation and reaches the local Spark pool; architecture and chat prompts reach
the cloud.

Still not done:

- No classification cache. The prefix hash the router already computes would
  key one, so a multi-turn conversation classifies once instead of per turn.
  Worth doing only if 115us ever shows up in a profile, which it has not.
- Routing on *difficulty* remains out of scope: the 96% GSM8K-versus-lookup
  separation most likely measures genre rather than difficulty.

## Fallback model (2026-08-07)

Every routing chain now ends at a deployment-wide fallback, set with
`PUT /admin/fallback-model`. Rule-level failover only reaches targets that rule
named, and a rule author cannot anticipate every way a chain runs out — every
backend unreachable, every provider rate-limiting, a model whose backends were
all dropped for an undecryptable credential. It applies to plain concrete model
names too, not only virtual ones.

Still subject to authorisation, like every other candidate: a caller never
granted the fallback does not reach it, so it cannot widen anyone's access.
Enforced by a partial unique index rather than convention, so "at most one
model is the fallback" cannot drift into two.
