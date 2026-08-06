# TODO

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
- **That ceiling is the upstream client, not this code.** Instrumenting
  frames-in vs frames-out gave a ratio of exactly **1.000**: a second frame is
  never already available when the first is consumed. `hyper-util`'s pooled
  client runs each connection in its own task and hands body chunks over a
  1-deep want/give channel, so every SSE frame costs a task wakeup, usually
  cross-thread. A profile agrees — ~28% of samples in `__psynch_cvwait`,
  `__workq_kernreturn` and `kevent`.
- **The request path is ~2% of a request.** All of this proxy's own per-request
  work measures ~0.76µs (URL format + `Uri` parse 156ns, bearer header 92ns,
  header copy 97ns, `BodyPeek` parse 229ns at 1KiB, prefix hash 108ns, path
  allocations 41ns) against ~38µs of core time per request. The rest is kernel,
  socket and hyper protocol work.

**None of this is urgent.** Ten vLLM nodes at 2000 tok/s each is ~20k frames/s
against a 650k ceiling — about 3% utilised — and the added per-token latency is
~1.5µs against inter-token gaps of ~500µs. Do these only if the proxy is
measurably in the way.

### Worth doing, hardest first

#### 1. Own the upstream connection instead of using the pooled client

Replace `hyper_util::client::legacy::Client` with `hyper::client::conn::http1`,
driven on the same task as the response body, plus a small connection pool of
our own (keep-alive, idle eviction, per-backend caps).

This is the only change that targets the actual ceiling: it removes the
per-frame cross-task handoff and makes frame coalescing possible for the first
time. It also means owning pooling correctness, which the current client gives
us for free.

**Unverified** — prototype against `scratchpad/bench` and confirm the frames/s
gain before committing to it.

#### 2. Byte-level relay after the response is committed

Once headers are forwarded the proxy never looks at the body again, so it could
copy raw socket bytes rather than decoding and re-encoding chunk framing. This
is as fast as a proxy gets.

The cost is connection reuse: without tracking chunk framing there is no way to
know where one response ends and the next begins, so it implies closing the
upstream connection per request unless the framing is tracked anyway. Probably
only worth it in combination with (1).

#### 3. Re-measure against a real vLLM node

Everything above assumes the mock's flush-per-token behaviour. Under real
batching the merge ratio is likely above 1.000, which would make coalescing
(see below) worth reviving on its own. Measure before building anything.

### Measured and rejected — do not retry without new evidence

- **Coalescing already-arrived frames in `TrackedBody`.** 1301 → 1318 req/s,
  merge ratio 1.000. Cannot help while the client hands over one chunk per
  round trip. Revisit only after item 1 or 3.
- **Hand-rolled `model` scanner to skip the JSON parse.** 67.1k → 67.2k req/s
  on 64KiB bodies. `serde_json` skips what it does not want at ~16 B/ns and the
  parse is ~3% of a request; a bespoke parser on the routing path is not worth
  the risk of misrouting.
- **Pre-parsed `Uri` per backend per endpoint.** ~0.2%, and it forces an
  endpoint-index coupling between `proxy.rs` and `registry.rs`.
- **Anything else on the request path.** There is under 1µs available in total.
