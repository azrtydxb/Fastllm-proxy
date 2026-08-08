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

## Tool calling through translated backends — done

Anthropic and Gemini backends now take `tools`/`tool_choice` and answer with
`tool_calls`, streaming included. `check_supported` refuses only the
deprecated `functions` spelling.

Three parts were not the rename they looked like:

- **Conversation history.** OpenAI puts a call on the assistant message and
  its result in a separate `role:"tool"` message keyed by id; both native
  protocols nest the result *inside* the following user message. So
  `split_system` became a real message mapper (`Turn`, `ToolResult`), and it
  carries an id→name map because Gemini pairs a result to its call by function
  **name** and never sees the id. Consecutive results fold into one message,
  which is what a model that called three tools in parallel gets back.
- **Streaming.** Anthropic sends `content_block_start`, then `input_json_delta`
  fragments of *partial* JSON, then `content_block_stop`. Those fragments are
  forwarded to the client unparsed — mid-call they are not valid JSON, and
  buffering until they were would defeat streaming. Anthropic's block index
  counts text blocks too, so it cannot be reused as OpenAI's `tool_calls[]`
  index; the translator hands out its own. Gemini needs none of this: it
  delivers a call complete in one event.
- **Two ids that do not exist.** Gemini supplies no call id, so one is
  synthesised from the request id — deterministic, so translation tests still
  assert exact bytes. And Gemini reports `finishReason: "STOP"` on the very
  event carrying a call, so the presence of a call outranks it; forwarding
  "stop" would tell the client the turn was over and the call needed no reply.

Two smaller things found while building it, both of which would have produced
a request the provider rejects with a 400 naming nothing: Gemini refuses
`additionalProperties` and `$schema`, which every JSON-Schema generator emits,
so they are pruned at every level; and `tool_choice: "none"` is expressed by
offering no tools at all rather than by a `{"type":"none"}` form newer than
the pinned Anthropic API version.

Verified end to end through a real proxy against a mock Anthropic provider
(`tests/native_protocols.rs`), both turns: the call comes back, and the
transcript carrying its result translates on the way in.

Nothing outstanding here: the two items that used to follow — multimodal, and
the three remaining providers — are both built. See below.

## Image and audio input through translated backends — done

`Content::into_text` flattened every message to a string, so a multimodal
request to a native backend was a 501. Parts now keep their order, because
order carries meaning: the same words before and after an image ask different
questions. Adjacent text still joins, so a text-only turn goes out as a bare
string exactly as before.

Media never causes a fetch. `data:` URLs translate inline with the base64
untouched; a remote URL is handed to Anthropic, which resolves it itself.
Downloading it here would be a network call while serving a request.

Two cases are refused by name rather than approximated: a remote URL for
Gemini, whose `fileData.fileUri` only addresses Google's own Files API, and
audio for Anthropic, which has no audio input at all. Media in a system prompt
or a tool result is still refused — neither protocol has a form for it, and
keeping only the text would discard the image being asked about.

## Cohere, Bedrock and Vertex — done

The entry that used to sit here said Bedrock needed SigV4 request signing and
that none of the three was reachable by configuration. That is no longer true,
and the correction is most of the work:

- **Cohere** ships an OpenAI-compatible endpoint at
  `https://api.cohere.ai/compatibility/v1`. A backend row, no code.
- **Bedrock** now serves the Chat Completions API at
  `bedrock-runtime.<region>.amazonaws.com/openai/v1` and authenticates with a
  Bedrock API key as a bearer token. No SigV4, no credential machinery, no
  code — a backend row.
- **Vertex AI** is the one that genuinely needed code, and it is the one the
  old entry described correctly: an OAuth2 access token that expires hourly.

Vertex is built as `credential_kind = 'gcp_service_account'` (migration 0016).
The column then holds the service account's JSON key file rather than a usable
credential, and `src/control/gcp.rs` exchanges it for an access token during
snapshot build — RS256 assertion signed with `ring`, posted to Google's token
endpoint over the crate's one shared HTTP client.

Three things decided the shape:

- **It belongs in the control plane.** Minting is a network call, and the
  request path performs no I/O. The control plane already rebuilds and ships
  snapshots on a schedule, so the token travels as an ordinary `api_key` and
  the data plane never learns the difference — no new hot-path field, no
  refresh timer in the proxy.
- **The cache is not optional.** Rebuilds run about every second; minting per
  rebuild would be thousands of token requests an hour to re-derive an
  hour-long value. Tokens are cached per service account and reused until five
  minutes before expiry.
- **Failures stay contained.** A revoked key drops that one backend with the
  reason logged, exactly as a failed decrypt does, rather than failing the
  whole rebuild and taking unrelated models offline.

Not verified against Google's live endpoint: this deployment has no GCP
service account. Everything short of that is tested against a local token
endpoint — a real RS256 assertion whose signature is verified against its own
key, the exchange, the error body surviving into the message, and the cache
both reusing a token and refusing to hand out one inside the refresh margin.

## The snapshot ETag could hide a config change indefinitely — fixed (2026-08-08)

A proxy could keep serving a stale configuration forever, with nothing in any
log to say so.

`Snapshot.version` was `EXTRACT(EPOCH FROM now())::BIGINT` — **whole seconds** —
and the same value is the `/snapshot` ETag. Two builds inside one second
therefore shared a version, and a proxy holding the first was answered `304 Not
Modified` for the second. Not a delay: it kept being answered 304 until some
later change happened to land in a different second.

Two things made same-second builds routine rather than rare: the rebuilder
ticks every second, and every admin write calls `refresh()` for an immediate
one. So an admin write landing next to a tick was the common case, not a race.

Found by investigating a symptom that looked like nothing at all — one proxy
of two rejecting a newly created API key for several minutes while its sibling
accepted it immediately, both reporting healthy and listing identical models
and backends. The models matched because a later, unrelated `api_base` change
bumped the version into a new second and unwedged it, which is also why it
"self-resolved" and could not be reproduced afterwards.

The stamp is now microseconds via `clock_timestamp()`, in a named
`snapshot_version` function that carries the reasoning. `clock_timestamp()`
rather than `now()` because `now()` is transaction time and would return one
value for every call inside a transaction — nothing here runs in one today,
which is the kind of assumption worth not depending on.

The test asserts on the stamp, not on two `build_snapshot` calls. The first
version of it did the latter and **passed against the whole-second stamp it was
written to catch**, because a full build takes long enough to straddle a second
boundary. Verified the current one fails against the old query and passes
against the new.

Diagnosis was much harder than it should have been because no proxy published
which snapshot it was serving: a lagging pod answers `/health` with `ok`, lists
the right models, and misbehaves only on the part that changed. `/health` now
carries `snapshot_version` and a key count, and `/metrics` exposes
`fastllm_snapshot_version`, so a fleet-wide `max() - min()` shows a stuck pod.

## Escalation costs 50-100ms in the container, not 3.3ms — open (2026-08-08)

Measured, not suspected: `fastllm_classify_duration_seconds` on the dev cluster
puts escalated classification in the 50-100 ms bucket, where `bench/minilm`
measured bge-small at 3.27 ms. The bench figure is a 10-core arm64 macOS host;
the deployed container is a 2-core limit on an 8-core arm64 k3s node.

The fast tier's ~115 µs holds — it is a memory lookup, not a matmul — so this is
specifically the transformer.

Docs corrected to state both numbers rather than the flattering one. Not
diagnosed further, and worth doing before a routing rule sends real volume
through tier 2. Three things to measure, in order of expected payoff:

1. **`intra_threads`.** `fastembed` leaves ONNX Runtime's intra-op thread count
   at `available_parallelism()`. If that reads the node's 8 cores rather than
   the cgroup's 2, eight threads contend for two cores and get throttled.
   `InitOptionsUserDefined::intra_threads` is the knob; one line, but it needs a
   measurement either side, not a guess.
2. **The core.** Some of the gap is simply that an arm64 k3s core is not an
   M-series one. Establishes the floor for the above.
3. **The mutex, which matters more at volume than either.** `Tier2` holds
   `Mutex<TextEmbedding>` because ONNX's session takes `&mut self`, so escalated
   classifications serialise. At 58 ms each that caps escalated throughput near
   17/s per pod however many cores it has.

Found only because the histogram existed. The mean read 191 ms, which looked
like the documented claim being wrong by 60x; the distribution showed two fast
classifications at 115-500 µs and one at 570 ms, which was the lazy model load —
a separate bug, now fixed by warming. What remained after that fix is this.

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

**Semantic routing** (embed the prompt, route by similarity) was rejected here
and has since been built — see "Semantic routing" below. The objection recorded
at the time was that an embedding per request would be a network call on the
hot path; the answer turned out to be an in-process static embedding at ~115µs,
which is neither a network call nor measurable.

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
