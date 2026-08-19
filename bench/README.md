# bench

Standalone measurement tools used to produce the performance numbers in the
root `TODO.md`. Not part of the shipped product and not built by a plain
`cargo build`/`cargo test` at the workspace root (see `default-members` in the
root `Cargo.toml`) — build or run them explicitly with `-p bench`.

These are throwaway instruments, not tests: no unit tests of their own, and no
assertions about pass/fail. Read the numbers, compare them to `TODO.md`,
re-measure before acting on anything there.

```
cargo build --release -p bench
```

produces `target/release/{upstream,load,realbench,micro,proto,tcprelay}`.

## What each tool measures

- **`upstream`** — mock vLLM-shaped server. Answers `/v1/models` immediately
  and streams SSE frames with no think time, so whatever sits in front of it
  is the only bottleneck being measured. `TOKENS` sets how many SSE events go
  in a response (0 means a single non-streaming JSON reply); `PER_FRAME` packs
  that many SSE events into each HTTP body frame, holding total bytes constant
  while varying frame count — this is how the frames-per-response sweep in
  `TODO.md` ("500 single-event frames → 77 MiB/s; 5 hundred-event frames →
  2918 MiB/s") was produced. `PORT` (default 8100) and `CLOSE_EVERY` (send
  `Connection: close` every Nth response, to exercise pool churn) round it
  out.

- **`load`** — closed-loop load generator. Opens `argv[2]` connections against
  `argv[1]` for `argv[3]` seconds, POSTing a chat-completion body of
  `argv[4]` bytes, and reports request throughput, time-to-first-byte
  percentiles, and response bytes/s. `MODEL_FIRST=1` puts `model` before
  `messages` in the request JSON, matching hand-written clients instead of the
  OpenAI SDKs' field order.

- **`realbench`** — the same idea as `load` but against a real backend
  (`argv[1]` = URL, `KEY`/`MODEL` env vars), reporting time-to-first-token and
  inter-token gaps rather than throughput: what a proxy costs a single stream,
  not how hard the proxy can be pushed. This produced the "measured against
  the real spark2 replica" numbers in `TODO.md`.

- **`micro`** — in-process microbenchmarks of the proxy's fixed per-request
  work (URL formatting, header copy, `BodyPeek` JSON parse, prefix hash,
  path allocations), replicating exactly what `src/proxy.rs` does on the hot
  path. Backs the "request path is ~2% of a request" numbers.

- **`proto`** — prototype proxy that owns its upstream connection and drives
  it from inside the response body's `poll_frame`, instead of using
  `hyper-util`'s pooled client. Existed to test whether replacing the pooled
  client would remove a per-frame cross-task wakeup; it did (see
  `src/upstream.rs` in the main crate, which is the production version of
  this idea). Also prints a frame merge ratio, used to confirm that no second
  frame is ever waiting once the wakeup is gone (measured ratio: exactly
  1.000).

- **`minilm`** — the one classifier instrument still here. Measures a
  candidate tier-2 (contextual) model against the splits a static embedding
  cannot do, on real labelled data: `python3 bench/fetch-prompts.py` first,
  then `cargo run -p bench --release --bin minilm <model-dir>`. The
  model-_selection_ experiments that chose `potion-code-16M` are gone; what
  they concluded is written down in `docs/classifier/measurements.md`, which
  is the part worth keeping.

- **`tcprelay`** — the ceiling: a dumb bidirectional TCP relay
  (`tokio::io::copy_bidirectional`, no HTTP parsing, no framing, no
  decisions). No proxy that speaks HTTP on this path can beat it, so it
  bounds what any byte-level-relay optimization could possibly be worth —
  see "Byte-level relay after the response is committed" in `TODO.md`.

## Reproducing the headline numbers

All tools default to `127.0.0.1` and are meant to run against each other on
one machine. A typical session, reproducing the frames-per-response sweep and
the proxy-vs-relay ceiling comparison from `TODO.md`:

```bash
# Terminal 1: mock upstream, 500 SSE frames per response, one event per frame.
TOKENS=500 PER_FRAME=1 cargo run --release -p bench --bin upstream

# Terminal 2: the proxy under test, pointed at the mock upstream (see the
# main README for config format), listening on :4000.
cargo run --release -- --config bench-config.yaml --port 4000 --role proxy

# Terminal 3: load generator, 32 connections, 5 seconds.
cargo run --release -p bench --bin load -- http://127.0.0.1:4000/v1/chat/completions 32 5

# Repeat with PER_FRAME=100 (same TOKENS) to see the per-frame-vs-per-byte
# split, and with PER_FRAME=1 pointed straight at :8100 to isolate the proxy's
# own overhead from the upstream's.
```

For the TCP-relay ceiling: run `upstream` as above, then

```bash
UPSTREAM=127.0.0.1:8100 PORT=4300 cargo run --release -p bench --bin tcprelay
cargo run --release -p bench --bin load -- http://127.0.0.1:4300/v1/chat/completions 32 5
```

and compare its req/s and MiB/s against the same `load` run pointed at the
real proxy on :4000.

For the fixed-cost breakdown: `cargo run --release -p bench --bin micro`
needs no server running — it benchmarks in-process.
