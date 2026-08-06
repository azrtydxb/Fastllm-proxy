# TODO

## P0: control plane and RBAC — done

Real per-key RBAC (principals, roles, permissions, SHA-256-hashed API keys),
a control/data-plane split (`--role=control`/`proxy`, sharing `Snapshot` as
the sole contract), three snapshot sources (file, http-poll, in-process),
`fastllm-proxy import` to migrate a `File`-mode deployment onto a database,
and the CI/deployment/docs wiring in this task. Design and self-review:
`docs/superpowers/specs/2026-08-06-control-plane-rbac-routing-design.md`.

Known gaps carried forward rather than silently fixed:

- **The admin API has no authentication of its own.** `/admin/*` and
  `/snapshot` are gated on the shared proxy token alone. Real admin auth
  (principals with Argon2id passwords, sessions) is specified but deferred to
  the management-UI phase below. Until then, `/admin` must never be exposed
  outside the cluster — see `deploy/README.md`.
- **`model_backends.upstream_api_key` is stored unencrypted at rest.**
  Recorded during the Task 9 import work (`migrations/0002_correct_upstream_api_key_comment.sql`);
  no encryption-at-rest layer exists in this codebase. Database read access
  is upstream-credential access.

Deliberately not covered by P0: rate limits (P2), usage and budgets (P3),
virtual models and routing (P1), the management UI (P4), and `POST /usage`
(the wire protocol has it; nothing sends to it until P2).

## Features

### Embedded management and monitoring UI

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
