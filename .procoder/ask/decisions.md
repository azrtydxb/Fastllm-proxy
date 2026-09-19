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
