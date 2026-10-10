# Changelog

Notable changes, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Commit bodies carry the reasoning and the measurements and remain the better
source for _why_ anything is the way it is; this file is the summary.

## 0.3.5 — 2026-10-10

### Fixed

- **The stall detector needs a baseline.** Counters that have never moved are
  an engine's normal, not a wedge: an audio model's text-token counters do not
  advance while it synthesises, and a vLLM TTS model keeps one request
  resident in its stage gauge forever, so every scrape read
  running-but-frozen. That backend was ejected ten seconds after every
  re-entry, on both replicas, forever; kw's `qwen3-tts-base` had been flapping
  on a five-minute cycle all day. A frozen sample now only counts once the
  engine has shown moving counters.
- **A traffic ejection is re-examined when the fleet is actively serving the
  backend.** Ejections stay per-replica — fast, local — but the fleet verdict
  now carries whether a peer has requests in flight, and that evidence, not
  mere reachability, can withdraw the verdict before the backoff runs out.
  Idle peers still cannot overrule a busy replica, and a genuinely dead
  backend re-ejects within one probe interval. This was the "different workers
  see different backends" instability: replicas held phase-shifted ejections
  for minutes, and which one failed a request depended on which proxy the
  Service picked.
- **Provider reachability classifies on a status tail, not digits in the
  message.** The old substring match read ports and body text — a mock on
  port :40353 classified as a 403, flaking the suite.

### Changed

- `src/registry.rs` gained the baseline guard and the serving-gated
  reconsider; `health_report.rs` carries the `serving` bit (serde-defaulted,
  so mixed versions still understand each other). Tests cover the audio-engine
  shape and the serving-fleet withdrawal.

## 0.3.4 — 2026-10-10

### Security

- **Outbound URLs are validated where they are stored.** A provider's
  `api_base`, an MCP server's URL, an A2A agent's URL and a GCP service
  account's `token_uri` are checked when written: a host resolving into a
  private, loopback, link-local or `0.0.0.0/8` range is rejected, so a stolen
  admin session cannot point the control plane at cloud metadata or another
  private network. Upstreams that are legitimately private — a proxy in front
  of its own cluster's engines is the normal case — are named in
  `FASTLLM_SSRF_ACCEPT` (CIDRs, or hostnames with `.`-prefix for subdomains);
  the kw manifest sets it for the cluster ranges. A bad entry is dropped with
  a warning rather than stopping startup.
- **The GCP `token_uri` check compares the URL host exactly and requires
  https.** The substring comparison it replaced admitted
  `https://evil.com/?x=oauth2.googleapis.com/token`, and plain http would have
  carried the RSA client assertion — a bearer credential — in the clear.
- **MCP tool calls and A2A agent RPC are rate limited.** They were exempt
  from the limiter, so a valid key could fire unlimited side-effectful
  operations upstream; both now consume the same token/request quotas as
  model traffic and answer 429 like it.
- **Line breaks are rejected in provider extra headers**, closing header
  injection through `HeaderValue::from_str`'s leniency.
- **A warning is logged whenever a legacy V1-encrypted credential is
  decrypted.** V1's version byte is not AEAD-authenticated; V2 is current,
  and the warning names the re-encrypt path.

### Changed

- `src/security.rs` is the new module; `normalise_api_base` and the MCP/A2A
  write routes call it. Tests cover the bypass shapes (substring hosts,
  lookalike domains, `0.1.2.3`-style `0/8`) and the allowlist matcher.

## 0.3.3 — 2026-10-05

### Changed

- **Every provider is named `<cluster>-<node>-<model>-<port>`.** Services
  advertised through `fastllm.io/advertise` were named `<cluster>-<port>`
  because the agent knew neither their node nor their model, so two engines
  on one port in one cluster collided and the second fell back to a bare
  address. The agent now takes the node from the pods a Service selects and
  the model from the first id `/v1/models` answers with, folds each part to
  lowercase `[a-z0-9.]` and dashes, and leaves out a part it cannot know.
  Names travel on every heartbeat, so upgrading the agent renames existing
  providers in place; routing follows model names, not provider names.

### Fixed

- **A provider deleted mid-sweep no longer fails the whole sweep.** The sweep
  read each provider's kind in a second query; one deleted in between (a
  lapsed lease, an operator's delete) found no row and aborted the sweep for
  every other provider. It is also why one control-plane test failed
  intermittently.

### Security

- **`fastllm.io/advertise` is honoured only for `http(s)` URLs.** urllib also
  opens `file://` paths, so anyone able to annotate a Service could otherwise
  choose what the agent reads.

## 0.3.2 — 2026-10-05

### Fixed

- **A host that swapped models is re-learned, not stranded.** When a Spark's
  engine went from qwen3.5-9b to the 35B on the same port, the sweep called it
  a mismatch and marked it degraded — and a degraded provider is one the sweep
  does not reconcile, so the registry kept routing qwen3.5-9b to an engine that
  no longer had it. The agent's heartbeat cleared the flag every thirty
  seconds, so the provider's models also flapped in and out of every snapshot.
  A dynamic provider's mismatch is now reported and resolved in the same pass;
  a static or cloud provider's still degrades and waits for a human.
- **The node agent finds engines that name their port only in a health
  probe**, as the audio.cpp servers (nemotron ASR, Breeze TTS) on kw do: the
  numeric port of an `httpGet` readiness, liveness or startup probe is read
  after `--port`.
- **Providers on one cluster get distinct names.** Under `--kubernetes` the
  name adds the node or Service — `kw-gx10-9c17-8000` — so two engines on one
  port on different nodes no longer collide.

## 0.3.1 — 2026-10-05

### Fixed

- **The node agent keeps its leases on a cluster.** Discovery probed every
  candidate in turn — on kw about 330 LoadBalancer and host-network ports,
  each allowed the full probe timeout — and lease renewal waited for it, so a
  pass took over a quarter of an hour against a 90-second lease. It only
  looked healthy because the Sparks' old host agents were renewing the same
  endpoint. Candidates are now probed concurrently (`--probe-workers`, 32; a
  pass on kw takes about 35 seconds), and leases are renewed every
  `--interval` from the last discovery's result while discovery runs on its
  own clock (`--discover-interval`, 60s).
- **`agent/kubernetes.yaml` starts.** It passed `--kubernetes` as `command`,
  which replaces the image's entrypoint, so the container tried to execute
  the flag; it is `args` now. It also carried NovaNAS's node name and
  advertise address, and now carries kw's, with the image pinned to the
  release and held there by the release-consistency test.
- **The registering-hosts doc describes the steps that work**: where the CA
  comes from, how to mint the agent's key (a principal's `sk-` key — the proxy
  token is refused), the `nodes` RBAC, and that the Sparks no longer run the
  systemd agent.

## 0.3.0 — 2026-10-05

### Added

- **The node agent ships as an image, and speaks Kubernetes.** The host agent
  — the thing that registers a machine's model endpoints with FastLLM and
  keeps the lease warm — is packaged as
  `ghcr.io/azrtydxb/fastllm-node-agent` (arm64 + amd64, built by the release
  job), and under `--kubernetes` it discovers what to register from the API
  instead of probing ports: exposed Services (NodePort, LoadBalancer) and
  hostNetwork pods, each addressed by its own node's InternalIP and accepted
  only if it answers `/v1/models`. `agent/kubernetes.yaml` runs it against
  kw, where it registers the engines running on the gx10 nodes. The
  registering-hosts operations doc carries the deploy steps.

- **Reasoning through `/v1/messages`.** A backend's `reasoning` /
  `reasoning_content` becomes `thinking` blocks, streamed as `thinking_delta`.
  Found running it against the live gateway: a reasoning model that spends its
  `max_tokens` thinking returns `content: null`, which came back as an empty
  message.
- **An Anthropic Messages frontend** (#30). `POST /v1/messages` with streaming
  SSE, `tool_use`/`tool_result`, images, `stop_reason` and `usage` (cached
  tokens included); `x-api-key` or bearer auth; `POST /v1/messages/count_tokens`
  as a local estimate; `GET /v1/models` in Anthropic's shape when the request
  carries `anthropic-version`. Translated onto the ordinary request path, so
  routing, budgets, rate limits and RBAC apply unchanged. Thinking blocks are
  dropped, and an Anthropic backend behind it is translated twice rather than
  passed through.

- **An id is a uuid, and no longer a count.** All 17 tables that have an
  identity migrate from `BIGSERIAL` to UUID v4 (migration 0041), generated from
  the live foreign-key graph rather than hand-typed and tested by migrating a
  restored dump of the running cluster. `/admin/provider-models/6351` used to
  say how many models a deployment had ever had and what the next one would be
  called; now that an id is the stable thing an operator and the API both hold
  on to, it should not also be a count. `audit_events` and `usage_events` keep
  a sequence id on purpose — theirs are cursors into an append-only log, and
  the audit listing pages with `WHERE id < $1 ORDER BY id DESC`, which means
  "newer than" only because the id is a sequence.
- **Names are editable in the UI.** Click one on Provider models or Frontend
  models. The hint differs between them on purpose: renaming a provider model
  is an internal relabel, while a frontend model's name is what clients ask
  for, so renaming it changes the deployment's public API — grants follow,
  callers do not.
- **Everything with a name can be renamed, and a rename no longer breaks
  links.** Provider models, frontend models, MCP servers, A2A agents,
  principals and roles all take a `name` on their PATCH route; principals and
  roles gain one. Renaming used to be impossible on purpose — three separate
  things recorded a model's name and every one of them would have broken.
  Targets now resolve by id and fall back to the recorded name, so a rename is
  followed automatically while a _deleted_ model still leaves a target naming
  what it wants (migration 0036's reason for existing); `relink_targets` puts
  the id back when that model returns. Grants move in the same transaction as
  the rename, because a `model/<name>` left behind revokes everyone holding it
  — which is what migration 0034 found by doing it. Usage history deliberately
  does not move: it records what a thing was called when the request was
  served.
- **Providers have names chosen by whoever knows what they are.** A cloud
  provider takes the vendor's name from the catalogue (`OpenRouter`, not
  `openrouter.ai`); a dynamic one is named by the agent registering it, sent on
  every heartbeat so changing `--provider-name` renames it; a static one is
  named by whoever adds it and can be renamed in place on the Providers screen.
  Safe because routing resolves a target by its **model's** name — the rename
  is carried onto `target_provider_name` so nothing is left naming a provider
  that no longer exists.
- **The Providers screen shows what kind each provider is.** A `static` /
  `cloud` / `dynamic` badge, which is the thing that decides whether anything
  may remove it. The two-letter tile that used to sit on each card is gone; it
  was the first two characters of a hostname, so every LAN provider showed
  `19`.
- **An endpoint can be handed to the agent on its host, and taken back.**
  `kind` on `PATCH /admin/providers/{id}`. Registration never converts a static
  provider into one that can expire, which is right — an agent must not be able
  to take over an endpoint a human typed in — but it left no way to opt in
  either: putting an agent on a host whose endpoints were already configured by
  hand registered them and changed nothing. Leaving `dynamic` clears the lease
  and any degradation with it, since the sweep reads `lease_expires_at`
  whatever the kind and an expired one would report the provider unreachable
  for ever.
- **The node agent can be told which CA to trust.** `--ca-cert`, and
  deliberately no `--insecure`: the bearer token it presents goes over that
  connection. Found by installing it on this project's own DGX Sparks, where
  the control plane's certificate comes from an internal CA and every
  registration failed on `CERTIFICATE_VERIFY_FAILED`. `agent/fastllm-node-agent.service`
  is the systemd unit those hosts now run.
- **Fixed: a UI deploy was invisible to anyone already using it.** `index.html`
  was served `no-cache` — "revalidate before using me" — with no `ETag` or
  `Last-Modified` to revalidate against, so browsers served the cached shell.
  That shell names content-hashed assets which are, correctly, cached for a
  year, so the old bundle kept loading and a hard reload was the only cure. The
  shell now carries the content hash `rust_embed` already computed, and answers
  `If-None-Match` with a 304 — which also delivers the transfer saving the old
  comment claimed but could not provide.
- **Fixed: a response from an upstream that hangs up was thrown away.**
  `Upstream::request` drives the connection itself, and treated the connection
  finishing as "no response came" — but polling the connection is what _reads_
  the response, so one delivered on the way out was discarded and reported as
  `upstream closed the connection before sending a response`. Any upstream
  answering `Connection: close` hit it, which is legal and happens under load.
  Found because a test stub did exactly that.
- **A provider reports what its engine is doing.** The sweep already dials each
  provider once a minute for its model list; on the same pass it reads
  `/metrics`, which vLLM and SGLang both publish, and records requests running,
  requests queued and KV-cache utilisation (migration 0044). The Providers
  screen said "1 of 1 up" for an idle box and one with forty requests queued
  alike. A provider that publishes no metrics — every hosted one — leaves the
  columns NULL and the row is simply not drawn, because absent and zero are
  different things.
- **`kind` says where a provider's details came from, not where its host
  lives.** `cloud` is preconfigured from the catalogue and needs only a
  credential; `dynamic` comes from an agent; `static` is one an operator typed
  in and filled out. A hand-typed address was being labelled `cloud` whenever
  its hostname was public, which claimed a provenance it did not have. The
  edit form offers the protocol for `static` only — the other two already know
  it.
- **A provider can be edited, and is dialled before it is saved.** Add, edit and
  remove on the Providers screen: name, address, protocol, kind, auth header and
  scheme, and the credential, all in one form opened by clicking the card. The
  endpoint is dialled before the row is written and nothing is stored unless it
  answers with its models — a rejected credential, an unreachable address and a
  path with no model list are all refused, the last because a 404 says the host
  is there and the path is not, which is overwhelmingly a mistyped address. The
  form shows the name, address, protocol and credential; the auth header, the
  scheme and the kind are how the credential is transmitted and which machinery
  maintains the row, neither of which is a question to put to whoever is adding
  a provider.
- **The catalogue covers every provider the docs name.** It held fourteen of
  the eighty `docs/providers.md` lists — the ones somebody had typed an address
  for — which made the Add provider dropdown read as the list of what FastLLM
  supports rather than the list of what had been seeded (migration 0042, with
  `tests/doc_claims.rs` now guarding the two together). Entries whose address is
  a fixed verified one carry it; the rest carry a `<placeholder>` the API
  refuses to store, which covers self-hosted engines and account-scoped
  endpoints. Five base URLs come
  from `go-ai-sdk` and thirteen from LiteLLM's own `openai_compatible_endpoints`,
  with the rest read off the vendor's documentation; `notes` records the source
  for each, so a moved endpoint has somewhere to go and check. The twenty-three
  placeholders that remain are the ones nobody but the operator can fill in:
  self-hosted engines and account-scoped endpoints.
- **Both dropdowns can be filtered.** The provider catalogue and the served-model
  list are eighty and four hundred entries respectively.
- **The model dropdown can be filtered.** A provider can serve several hundred
  models — OpenRouter answers with upwards of four hundred — and scrolling to
  one whose name you already know is the slowest way to pick it. The count
  reads `12 of 431` while a filter is on, and the chosen model stays in the
  list even when the filter would drop it, because a select whose value is not
  among its options renders blank and reads as "your choice was lost".
- **Adding a model starts from the provider that serves it.** An **Add model**
  dialog on the Provider models screen: pick a provider, and it reads that
  endpoint's `GET /v1/models` and offers what it serves, filling the local name
  in from the one you choose. It replaces a form that asked for a name first
  and an address afterwards — an order that required knowing the upstream
  model's name from memory before anything had offered it, and left a model
  routing nowhere in between. The two writes are one intent: a failed attach
  removes the model created a moment earlier rather than leaving a name that
  routes nowhere and blocks the retry with a duplicate-name conflict.
- **The catalogue says which credentials a vendor takes.**
  `provider_catalogue.credential_kinds` (migration 0040), returned by
  `GET /admin/provider-catalogue`. Only Vertex AI accepts anything but a static
  key, so the Add provider form asks that question only where there is an
  answer to give rather than putting a Google-shaped dropdown in front of
  someone adding Groq — and the UI reads that from the catalogue instead of
  naming a vendor in a component.
- **Providers can be added, edited and credentialled from the UI.** `POST
/admin/providers` and `PATCH /admin/providers/{id}`, and an **Add provider**
  form on the Providers screen with two ways in: a cloud vendor picked from the
  catalogue, which fills in its base URL and the header it wants its key in, or
  a typed address for anything else. Before this a provider could only appear
  as a side effect of attaching a backend, so the endpoint's credential had to
  be typed on a model form, and a provider with no models yet — the state you
  are in while deciding which of its models to serve — could not be expressed
  at all. Rotating a key is one write on the provider card.
- **Attaching a model starts from its provider, and offers what that provider
  serves.** `POST /admin/provider-models/{id}/backends` takes a `provider_id`,
  and the Provider models screen browses `GET /v1/models` on the chosen
  provider so an upstream name is picked from what the endpoint actually
  answers with rather than typed from memory. Already-registered models are
  marked. Endpoint fields alongside a `provider_id` are refused rather than
  ignored: the caller would otherwise believe they had set a credential there
  while the provider's is what gets sent.
- **A provider is a record.** It was a grouping the UI invented at render time,
  so nothing could register, count or refer to one, and the 80 providers in
  `docs/providers.md` had nothing to attach to. `providers` now owns the
  endpoint, the credential, the protocol and the auth scheme; a provider model
  belongs to exactly one provider and `model_backends` is gone (migration
  0029). Rotating a shared key is one write where it was one per model.
- **Hosts that serve models can register themselves.** `agent/fastllm-node-agent.py`
  registers an address on a lease and heartbeats; the control plane calls
  `GET /v1/models` itself, so discovery and reachability are the same test and
  a model the proxies cannot dial is never registered. Every engine answers
  that one call, so there is no engine matrix and no container mode. See
  `docs/operations/registering-hosts.md`.
- **Providers are probed for identity, not just liveness.** One call per
  provider answers both "is it reachable" and "is it still serving what is
  registered against it". The second is the drift that motivated this: a host
  answering happily while serving a different model than the row claims, which
  a health check reports as healthy. A dynamic provider that stops answering
  degrades first and is deleted only after 30 minutes — longer than a model
  load, because suppressing routing is reversible and deletion is not.
- **Usage survives the model that served it.** `usage_events` records the model
  and provider name at ingest and the foreign key is nullable, so deleting a
  model no longer erases what it was billed for (migration 0031). The hourly
  rollup is keyed by name for the same reason — its `model_id` was `NOT NULL`
  and part of the primary key, so a deleted model would have failed the whole
  retention batch.

- **Load balancing is per provider model, not per process.** `--policy` was a
  deployment-wide flag, which is the wrong shape the moment one control plane
  serves both kinds of pool — two identical local replicas sharing a prefix
  cache want `cache-affinity`, three hosted providers of differing speed want
  `lowest-latency`, and a flag can only be one of them. Each provider model may
  now carry its own (migration 0028, `policy` on `POST`/`PATCH
/admin/provider-models`, a control on the **Provider models** screen). Unset means the deployment
  default, so an existing database behaves exactly as it did.
- **The price sync can replace a price that is already set.** It never
  overwrote by design — a negotiated rate must not be replaced by a list
  price — but that left a model priced _wrongly_ unreachable from the UI,
  including one sitting at `0`, which reads as free. The preview now has a
  "replace prices that are already set" toggle, off by default, that
  re-previews as it changes.

### Changed

- **Two words for two things: provider model and frontend model.** A provider
  model is what a request is routed _to_ — one name on one provider, since the
  same model on two hosts is two provider models. A frontend model is what a
  client asks _for_: rules and weights resolving to a chain of them, and the
  only name a client is meant to use. The schema, the API, the UI and the
  documentation all use those words now: `provider_models` and
  `frontend_models`, `/admin/provider-models` and `/admin/frontend-models`
  (migration 0033). The old routes are gone rather than aliased — this rides
  the breaking change the provider split already made instead of adding a
  second one later for a cosmetic reason.
- **Access is granted on frontend models.** A request naming one is authorised
  against it; naming a provider model directly still needs a grant on that
  model. The old rule required a grant on the resolved provider model, which
  pinned every grant to a provider model's _name_ — so renaming one revoked
  access silently, as migration 0029 demonstrated. A grant on a frontend model
  covers the chain it routes to, so adding a target extends the reach of
  everyone holding it; editing one requires `config:write`, which already
  grants everything.
- **Declared context windows never reached routing.** `Registry` carried a
  `context_length` map whose doc comment said it was filled from the snapshot,
  and nothing ever filled it — so `routing::candidates`' context-window
  fallback, which demotes a model whose window provably cannot hold the
  request, could not fire in any production build. The column, the admin API
  field and the routing code were all present and correct; only the wiring
  between them was missing.

- **The Kubernetes operator earns its keep.** It was removed earlier in this
  cycle for reconciling two Deployments a chart already produces; it is back
  because the four things a chart genuinely cannot do are now implemented and
  verified against a live cluster:
  - **Ordered upgrades.** The two planes share a database schema, so
    `spec.image` rolls the control plane _first_ and holds the gateway at the
    image it is running until that has finished. Verified with a deliberately
    unpullable tag: the control plane went down, the gateway kept serving on
    the old image, and the `Upgrading` condition said which and why.
  - **Rotation that takes effect.** Both pod templates carry a hash of the
    resolved Secret material, so rotating the proxy token — or cert-manager
    renewing the control-plane certificate — rolls the pods instead of
    silently doing nothing until an unrelated restart.
  - **Preflight.** Every referenced Secret is resolved and checked before
    anything is applied; a missing key or a short encryption key becomes a
    condition naming the Secret and the key rather than pods in
    `CreateContainerConfigError`. `encryptionKey` is immutable, enforced by
    the API server through a CEL rule.
  - **A finished install.** `bootstrap` runs `set-password` as a Job once the
    control plane is ready, so the deployment ends with a UI that can be
    signed into. Verified end to end: `POST /login` returns 200 and the admin
    API answers with the cookie, 401 without.

- **The management UI knows when an operator runs it.** A **Deployment**
  screen — image, replicas, policy, timeouts, workers, pool size, autoscaling,
  plus phase, conditions and what is actually serving — that patches the
  `FastllmProxy` and lets the operator roll it out. It appears _only_ under an
  operator: the control plane learns it is managed from an environment
  variable only this controller sets, so a Helm or manifest install has no
  such screen and `GET /admin/deployment` answers 404. The control plane
  reaches the API server through its own ServiceAccount and a Role naming one
  `resourceName`, with `get` and `patch` and nothing else.

  Plus the day-1 fields a real cluster cannot do without — Service
  annotations (a pinned load-balancer address), scheduling, ingress, HPA,
  `workers`/`poolMaxIdle`, OTLP, a ServiceMonitor, and `extraArgs`/`extraEnv`
  as the escape hatch — and, for the operator itself, leader election over a
  Lease (so it runs two replicas rather than one), Kubernetes Events, and its
  own `/metrics`, `/healthz` and `/readyz`.

### Removed

- **Seven of the classifier benchmarks** (`potion`, `potion-real`,
  `potion-classes`, `potion-arch`, `potion-wide`, `classcheck`, `wrapskew`).
  They answered "which model, which classes, which token cap" once; the
  answers are in `docs/classifier/measurements.md`, which is the artefact
  worth keeping. `bench/minilm` stays — measuring a _candidate_ model is a
  question that recurs.
- **`docs/superpowers/`** — pre-build design notes and task lists for work
  that shipped, unpublished by the book and already contradicted by the code
  (they describe a third snapshot source that does not exist).

### Fixed

- **Stability under load** (audit of 2026-09-30). Each item is a mechanism
  found by reading the code, fixed without a reproduction:
  - A backend ejected by _traffic_ (header timeouts, a frozen engine) is out
    for 30s doubling to 5m, and neither a passing `/models` probe nor the
    fleet's vote can lift it early. A wedged engine answers `/models`, so the
    probe used to re-admit it at once and it cycled at the probe interval. It
    returns on probation: one more timeout and it is out again, for longer.
  - The stall detector needs 10s of frozen counters, not 4.
  - Health probes run once per endpoint, not once per model behind it. A
    provider with ten models was probed ten times per interval per replica.
  - The admission queue count no longer leaks when a client hangs up while
    waiting, which used to shrink `max_queued` until the gate refused
    everything.
  - An upstream body that goes silent for 5 minutes is abandoned instead of
    holding its slot forever.
  - The listener sheds connections past 20,000, times out slow headers (30s)
    and backs off on `accept` errors instead of spinning.
  - The disguised-error check (#28) reads at most 64 KiB and replays the rest.
  - Peer forwarding uses the request's own path and has a timeout.
  - Spend inside a 5% band is no longer a snapshot change, so a usage flush
    stops making every proxy rebuild routing and clear its cache.
  - Usage batches the control plane could not take are retried (bounded)
    instead of dropped, and flushed on shutdown.
  - A snapshot is applied only if its routing table builds: the table is
    built first, so a failure no longer leaves auth on the new policy and
    routing on the old one with no retry.
  - A snapshot with the same policy under a new version replaces the stored one
    without rebuilding the registry or clearing the cache.
    `content_eq` now also compares MCP servers, agents, prompt classes and the
    fallback model, which it used to ignore.
  - Snapshot builds that publish are serialised, so a slow periodic build can
    no longer store an older view of the database over a newer admin write.
  - The provider sweep and the usage roll-up run on one control-plane replica
    at a time (a Postgres advisory lock), so a second replica is safe.
  - Request bodies share a process-wide budget (`--max-inflight-body-mb`,
    default 512); a request that cannot get its share in 5s is refused with 503.
  - Retries onto a sibling backend are capped at a fifth of recent requests
    (plus a floor of 10 per 10s). A slow backend used to push its whole load
    onto its siblings at the moment they could least take it. A request
    refused a retry gets what it already got; failover to the next _model_ in
    a chain is not limited.
  - `fastllm_backend_ejections_total`, and `deploy/monitoring.yaml` with a
    ServiceMonitor and alerts for replica disagreement, flapping, no backends,
    dropped usage and upstream 5xx.
  - Readiness is `/readyz`, failing only while draining, with
    `--shutdown-delay` (5s) of continued serving after `SIGTERM`. It no
    longer tracks backends.
- **An upstream error sent with HTTP 200 is now returned with its real status**
  (#28). A non-streaming JSON `200` whose body is `{"error":…}` (a gateway in
  front of NVIDIA, saturated) takes the `error.code` it declares, so clients
  retry, failover fires and error counters see it. Streams are never read.
- **A backend a probe ejected is the last resort, not a 502** (#29). When a
  model's every backend is out of rotation and nothing is left in its chain,
  the least-loaded one is tried anyway. The probe timeout is
  `--health-timeout` (default 3s) and the header timeout `--upstream-timeout`
  (default 120s); both were already flags.
- `POST /admin/models` silently dropped `context_length`: the field was
  PATCH-only, so a caller who sent it at creation got 201 and a model with no
  context window. It is settable at creation now.
- **Every request struct in the admin API now rejects fields it does not
  model.** serde drops unknown fields by default, which is how the above went
  unnoticed — and turning strictness on immediately found two more callers
  sending `roles` to `POST /admin/principals`, an endpoint that has never
  taken one. One of them passed `["inference"]` and believed it granted a
  role.

## [0.2.0] — 2026-08-14

Two new gateways — tool servers and agents — behind the same keys, the same
grants and the same accounting as models. Plus three importer bugs found by
running it against a real database rather than trusting the tests.

Published as `ghcr.io/azrtydxb/fastllm-proxy:v0.2.0` and
`ghcr.io/azrtydxb/fastllm-operator:v0.2.0`, **linux/amd64 and linux/arm64**.
Each architecture is built on a runner that is that architecture and the two
digests are merged into one manifest — the first attempt emulated amd64 with
QEMU and `rustc` segfaulted before compiling anything.

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
  between 0.3 and 1.0 is deliberately not built — see docs/agents.md.
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
a description of what 0.1.0 _is_ rather than a diff against something
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
  chain across _models_, not just replicas.
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
