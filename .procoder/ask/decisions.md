## Which CI build-time fix, if any, should I make?

A one-line CSS change costs ~25 minutes: `test` ~10 min, then `publish`
~12-17 min, run in series because `publish` needs `[test, ui]`. The `ui` job,
the only one that could have failed on that diff, took 1 minute.

Causes, measured: no cache for `~/.cargo` or `target/` (the only `actions/cache`
in the workflow is for the classifier model, and ARC runners are ephemeral
pods), so Rust compiles cold three times per run — clippy, clippy with
features, then test. The Dockerfile then compiles it a fourth time, in release
mode for arm64, with no dependency layer to survive a source change.

Options:

- **Cache cargo and `target/` in the `test` job.** Biggest win for the least
  risk; warm runs drop to a couple of minutes. Helps every run, Rust or not,
  and restructures nothing.
- **Split the Dockerfile's dependency layer** (cargo-chef or a manifest-only
  pre-build) so a source change recompiles this crate rather than the whole
  tree. Touches the image build every deploy depends on.
- **Skip the Rust job when no Rust changed** (`paths-filter`). A web-only
  commit becomes ~1 min plus the image build. Needs a `needs:` restructure,
  not just a skip: the UI is embedded via `rust-embed`, so the image must
  still be rebuilt.
- **Leave it alone.** 25 minutes is tolerable and the pipeline is understood.

**Decided:** cache cargo and `target/` in the `test` job. The other two remain open.

## Deploy the pool-policy fix to kw?

The LB pool feature has a bug: pool policies (e.g. `cache-affinity`,
`least-loaded`) were wired into the failover order but never reached
`Router::pick`, so backends always used the deployment's `--policy` for
dispatch regardless of what the pool configured.

Fix: walk frontend model targets in `build_snapshot`, collect pool→member→policy
mappings, write into `ModelDef.policy` so the registry's `PoolInner.policy` is
set correctly. New test verifies a `least-loaded` pool member gets that policy
in the snapshot. 397 tests pass, gate clean.

## Out-of-band access to the DGX Sparks now that wifi is off

Both nodes are ethernet-only: `piwifi` autoconnect set to `no`, the radio
disabled, and `wlP9s9` down on each. That removes the failure mode where a
wired fault silently demotes an inference node to a 2.4 GHz link and it keeps
serving, slowly, with nothing alerting.

It also removes the only network path in. A switch-port or cable fault on
`enP7s7` now means a console or physical trip, on a pair of boxes that already
need a power cycle when they thrash on memory.

Options:

- **Leave it as done — ethernet only, radio off on both.** Matches the request
  exactly. No emergency path.
- **Re-enable the radio on one node with `ipv4.never-default yes`.** Keeps a
  way in if the wired link dies, while the wifi can never install a default
  route, so it cannot silently carry traffic. Half the fleet stays reachable.
- **Re-enable on both with `never-default`.** Emergency path everywhere, same
  no-default-route guarantee, but two hosts hold a second address on the /24
  again.

**Decided:** leave it as done — ethernet only, radio off on both. Recovery from a
wired fault is a console or physical trip, accepted deliberately.

## Commit and deploy the unhealthy-backend failover fix?

A router-level bug caused a completely dead model to block failover: `Router::pick`
returned an unhealthy backend instead of `None`, so `proxy_request` never moved to
the next model or the deployment-wide fallback. `tests/failover.rs` already covers
the end-to-end path; unit tests all pass and the gate is clean.

Options:

- **Commit and push, then deploy to kw.** Straightforward — the fix is small and
  tested.
- **Defer for now.** No one has filed a bug yet, but the behaviour is silently
  broken and the fix is ready.

**Decided:** commit, push, deploy.

## Pull in the next ChatGPT OAuth story?

Story 1 (admin API accepts `credential_kind: chatgpt_oauth`) is closed. Sprint
`001-add-chatgpt-oauth-provider-credential-kind` is active with 11 remaining
stories. The logical next story is:

- **Task 4 — OAuth connect/status/disconnect APIs** (control plane: PKCE, token
  exchange, encryption, storage)

Or we could continue with the simpler stories first:

- **Task 5 — Rust token refresh** (snapshot build decrypts tokens, refreshes
  if within 5 min of expiry)
- **Task 6 — Health check / error handling** (401/403/429 from OAuth backends
  mark unhealthy → failover; already mostly existing code)
- **Task 7 — Tests** (unit tests for token parsing and refresh timing)

Options:

- **Pull in Task 4 (OAuth APIs) first.** The big piece: new module
  `src/control/oauth.rs`, three new endpoints, full PKCE + token exchange.
  Everything else depends on having tokens to decrypt.
- **Pull in Task 5 (token refresh) first.** Simpler: just a check and exchange
  in the snapshot build. The OAuth connect flow still needs the endpoints, but
  the data plane can start working on receiving tokens.
- **Pull in Task 6 (health/error) first.** Almost no code — just verify existing
  401/403 handling works for OAuth backends. Quick win, builds confidence.
- **Defer for now.** The foundation is laid (DB, schema, API validation). Come
  back when you want to actually connect a ChatGPT account.

- [decision] decisions.md Commit ChatGPT OAuth sprint 002

11 of 12 stories closed. 1 deferred (UI status — backend API ready). Tests pass (392/0). Gate clean (0 blocking).

Options:

- **Commit, push, and deploy to kw.** Sprint complete. The OAuth provider is fully usable on the control plane (connect, token refresh, disconnect, health).
- **Hold for now.** Wait for something else to batch with.

**Decided:**

## Fix four filed GitHub issues before committing sprint changes

Four open issues were raised before the sprint commit:

- **#21** — Headers-timeout failover re-dispatches full body → retry amplification (fixed: timeout-rate breaker + threshold config)
- **#18** — `/v1/models` strips `max_model_len` metadata (fixed: field added through model chain)
- **#19** — Health sweep can't detect vLLM engine-core deadlocks (in progress: needs `/metrics` stall detection)
- **#20** — Per-backend upstream timeout (in progress: needs per-backend config field)

Options:

- **Fix all four now.** They're all in the data plane and orthogonal to the ChatGPT OAuth work that is already committed. The remaining two (#19, #20) touch `registry.rs`, `state.rs`, `proxy.rs`, and the config schema.
- **Defer #19 and #20.** #21 and #18 are done and can be committed separately; #19 requires engine-scrape integration and #20 requires DB migration plus admin API changes, both larger undertakings.

**Decided:** fix all four before committing sprint changes.

## The pool member picker now caps every pool at one member

`3a3b0c2` scoped pool members to "providers of the pool's model". But a pool
member is a `provider_model_id`, and since migration 0045 one model is one row
carrying many backends. So:

- **Create form:** pick a model, and exactly one checkbox appears (verified —
  the interaction harness reports `found 1` and posts a single
  `provider_model_id`).
- **Existing pool:** `available` filters to models whose name matches the
  current member, and that single row is already `inPool` — so the add-member
  list is empty forever.

A one-member pool has nothing to choose between, so pools are currently inert.
The field's own hint ("tick every provider of this model to pool between them")
describes behaviour the schema cannot express: there is no way to name a single
provider as a member.

`6ea7f29` (pool policies reaching `Router::pick`) is sound and passes against
real Postgres — that one is worth deploying either way.

Options:

- **Revert the picker to multi-model, deploy the rest.** Restores pools that
  choose between models, which is what the schema supports and what the docs
  describe. Provider-level balancing within a model stays the model's own
  policy, which `6ea7f29` has just made work.
- **Make members provider-level for real.** A migration so a member can name a
  backend, plus API and UI. Matches the new hint text, but it is a schema
  change and a bigger piece of work.
- **Deploy as-is.** The pool-policy fix lands; pools stay single-member and
  inert until the picker is revisited.

**Decided:** pending.

## Nexus is full, which blocks CI and the deploy

`nexus-data-iscsi` is 200Gi and 100% used — 185.6G of it one `default` blob
store. Nexus logs `No space left on device` and its H2 database has closed, so
every repository returns 500: cargo (CI `test`), npm (CI `ui`), and the Docker
registry. That is the single cause of every CI failure since `4e8bdab`, not
anything in the code.

It also means the cluster cannot pull `fastllm-proxy` images at all — the
manifest for the _currently deployed_ `sha-4e8bdab` 500s. Running pods are fine
on their local cache; a restart onto a cold node would not be.

The image for `f2e8b43` builds cleanly on kw's buildkit — it got as far as
exporting the manifest, and only the push failed.

Options:

- **Expand the PVC.** `truenas-iscsi` has `allowVolumeExpansion=true`, so this
  is an edit plus a Nexus restart. Fastest way back to a working CI and a
  normal deploy, and it does not delete anything.
- **Reclaim space inside Nexus.** Cleanup policies plus "Compact blob store".
  Fixes the cause rather than the symptom, but it deletes artefacts and needs
  decisions about what may go.
- **Bypass the registry for this deploy.** Export the built image from buildkit
  and import it into containerd on the worker nodes, with
  `imagePullPolicy: IfNotPresent`. Gets `f2e8b43` running today, but the image
  exists only on the nodes imported to, so a reschedule elsewhere fails — and
  CI stays broken either way.

**Decided:** expand the PVC. Done — 200Gi → 300Gi, Nexus restarted to complete
the filesystem resize, now 66% used with 94G free. cargo, npm and the registry
all answer again, CI went green, and `sha-f2e8b43` is deployed.

## `npm test` inside the image build keeps exhausting inotify

The `publish` job failed with `EMFILE: too many open files, watch '/web'` —
vite's file watchers hitting `fs.inotify.max_user_instances`, which is at the
kernel default of **128** on the runner node (`max_user_watches` is fine at
249535). A re-run passed, so it is load-dependent rather than deterministic,
and it will recur.

The suite being run there is the same one CI's `ui` job already ran and passed
in the same workflow — the Dockerfile's `RUN npm test` is a second execution of
it, and the only thing that distinguishes it is that it can fail this way.

Options:

- **Drop `RUN npm test` from the Dockerfile.** Removes the duplicate run and
  the failure mode with it. `ui` still gates every commit, so nothing goes
  unverified; the image build stops re-proving what the pipeline just proved.
- **Raise `fs.inotify.max_user_instances` on the runner nodes.** Keeps the
  in-image test as a belt-and-braces check for anyone building the Dockerfile
  outside CI. A node-level sysctl, so it needs to survive node rebuilds.
- **Both.** Drop the duplicate run _and_ raise the limit, so other watcher-heavy
  builds on those nodes stop being fragile too.
- **Leave it.** Re-running the job clears it when it happens.

**Decided:** drop `RUN npm test` from the Dockerfile (`ed15a5d`). CI's `ui` job
still runs the suite on every commit and `publish` needs it, so nothing goes
unverified; a hand-built image is the only thing that loses the check, and the
comment there says so. The sysctl is left alone.

## Pool members should be backends of one model, not the model

The MEMBERS list shows one row per model (`qwen3-6-35b-a3b-nvfp4`, with its two
providers printed as a label). It should show one row per backend —
`…245:8000` and `…246:8000` as separately tickable — so a pool balances across
a chosen subset of one model's providers.

A member is a `provider_model_id` today, and the routing chain is model names
end to end: `expand_pool` returns `Vec<String>`, and the registry keys its
pools by model name. Nothing downstream can name a single backend, so this is
not a UI change.

What it takes, and it is all of these or none:

- a migration adding `model_backend_id` to `model_pool_members`, and a
  uniqueness rule that allows the same model twice with different backends;
- the admin API accepting a backend on the member;
- `build_snapshot` emitting the pool as its own `ModelDef` carrying exactly the
  ticked backends and the pool's policy, so the registry balances within it;
- `expand_pool` returning the pool's own name for such a pool;
- the UI listing backends once a model is chosen.

Checked and clear: `/v1/models` lists frontend models only, and authorisation
is on the client-facing name, so a pool-scoped `ModelDef` leaks into neither.

Two consequences worth deciding on rather than discovering:

- usage rows would attribute to the pool name rather than the provider model,
  because that is what served the request;
- pools of _different_ models (failover between `qwen3.5-9b` and
  `qwen3-8-27b`) and pools of _one model's backends_ become two behaviours in
  one table. They can coexist — a member with a backend means the new one — but
  it is two things to hold in your head.

Options:

- **Build it.** The five pieces above, with tests at each layer.
- **Show the backends, keep the member a model.** After picking a model, list
  its backends read-only so it is visible what the policy will balance across.
  No schema change, and it matches what routing can express today — but you
  cannot exclude one provider.
- **Leave it.** Pools stay model-level; provider balancing is the model's own
  policy, which `6ea7f29` made work.

**Decided:** build it. Members become backends of one model.
