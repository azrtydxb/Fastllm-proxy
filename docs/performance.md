<!-- Measured numbers and their conditions. The README carries only the
headline comparison; everything that needs a paragraph of caveat lives here. -->

# Performance

Every number below was measured, on the hardware named, on the date named.
Nothing here is projected or scaled from a smaller test. Where two runs of the
same thing disagree, both are shown.

## Against LiteLLM, in pictures

Measured 2026-08-07 on an arm64 Kubernetes cluster. Both gateways: one
replica, 4 CPU / 6 GiB, same idle node, reached over NodePort, same two vLLM
backends, interleaved A/B runs. LiteLLM ran 4 uvicorn workers in `PRODUCTION`
mode. Manifests: [`bench/compare/`](https://github.com/azrtydxb/Fastllm-proxy/tree/main/bench/compare).

### With the GPU removed — what the gateway itself costs

A mock upstream that answers instantly, so the gateway is the only thing being
measured.

<picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-mock-throughput-dark.svg"><img alt="Requests per second against a mock upstream. fastllm-proxy climbs to roughly 500-635 per second; LiteLLM plateaus near 36." src="images/bench-mock-throughput-light.svg" width="49%"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-mock-latency-dark.svg"><img alt="Median time to first token against a mock upstream, log scale. fastllm-proxy stays between 8 and 46 milliseconds; LiteLLM rises from 87 to 1313." src="images/bench-mock-latency-light.svg" width="49%"></picture>

**~15x the throughput and 10-28x lower latency**, and the gap widens with
concurrency rather than narrowing. This is the _ceiling_ on what the choice can
be worth, and you collect it only when the GPU is not your bottleneck — which
is the next section, and the more honest one.

### With real GPUs — throughput is a wash, consistency is not

Two vLLM replicas, 16 concurrent slots each. A gateway that balances correctly
should climb to 32 concurrent streams and then flatten. Both do, and land on the
same ceiling.

<picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-real-throughput-dark.svg"><img alt="Aggregate tokens per second against two real vLLM replicas. Both gateways climb together and flatten at about 305 to 332 tokens per second from 32 concurrent streams onward." src="images/bench-real-throughput-light.svg" width="49%"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-real-latency-dark.svg"><img alt="Time to first token against real vLLM, p50 solid and p99 dotted. Medians track closely; the 99th percentile diverges sharply at 32 streams." src="images/bench-real-latency-light.svg" width="49%"></picture>

**Aggregate throughput is a wash** — both saturate the same GPUs, and at a
single stream LiteLLM won several rounds outright. If your bottleneck is the
GPU, the gateway barely moves your token rate.

What differs is steadiness. At 32 streams, p99 time-to-first-token is
**766 ms against 2921 ms**, and the gap between consecutive tokens is 15-25%
less variable at _every_ concurrency level:

<picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-real-jitter-dark.svg"><img alt="Standard deviation of the gap between tokens against real vLLM. fastllm-proxy is consistently 15 to 25 percent lower than LiteLLM at every concurrency level." src="images/bench-real-jitter-light.svg" width="49%"></picture>

A p50 that moves by 20 ms and one that moves by 280 ms are different products
even when their medians match.

### Against a real vLLM, the proxy is not measurable

Two runs against the live spark2 replica (arm64, `qwen3-6-35b-a3b-nvfp4`),
2026-08-06. "Direct" is the same client against the same vLLM with no proxy in
the path:

|                                 | through the proxy | direct      | delta      |
| ------------------------------- | ----------------- | ----------- | ---------- |
| TTFT, run 1                     | **83.2 ms**       | 83.8 ms     | −0.6 ms    |
| TTFT, run 2                     | **90 ms**         | 93 ms       | −3 ms      |
| inter-token                     | **27.51 ms**      | 27.46 ms    | +0.05 ms   |
| 8 concurrent streams, aggregate | **121.8 tok/s**   | 118.7 tok/s | +3.1 tok/s |
| 8 concurrent, inter-token       | 63.4 ms           | 62.2 ms     | +1.2 ms    |

The proxy comes out marginally _ahead_ on two of these, which is not a claim
that a proxy makes inference faster — it is run-to-run noise on a live GPU, and
that is the point: the overhead is below the noise floor of the thing it sits
in front of.

### Where the time actually goes

The proxy's own per-request work totals **~0.76 µs**, against ~38 µs of core
time per request. Roughly 2%; the rest is kernel, socket and HTTP protocol work
that any process doing this job would pay.

| step                          | cost   |
| ----------------------------- | ------ |
| URL format + `Uri` parse      | 156 ns |
| `BodyPeek` parse (1 KiB body) | 229 ns |
| prefix hash for affinity      | 108 ns |
| header copy                   | 97 ns  |
| bearer header build           | 92 ns  |
| path allocations              | 41 ns  |

Authorisation, rate limiting and budget checks are not in that table because
they are set lookups against a pre-flattened in-memory snapshot — no database
call, no network call, no file read on the request path. `tests/no_io_on_hot_path.rs`
fails the build if that changes.

### Synthetic ceilings

10-core arm64 macOS, `--release`, driven by a mock SSE upstream that flushes
every frame with no think time. That is the **worst case** for framing
overhead — a real vLLM emits one SSE event per HTTP frame tens of milliseconds
apart, so production numbers are better than these.

|                                | measured                   |
| ------------------------------ | -------------------------- |
| streaming                      | **7,921 req/s**, 471 MiB/s |
| non-streaming, 64 KiB bodies   | **67,200 req/s**           |
| frame ceiling (any frame size) | ~650,000 frames/s          |
| raw byte pump                  | ~3 GB/s                    |

### What one architectural decision was worth

The upstream client used to be a pooled `hyper_util` client, which cost one
cross-task wakeup per frame. Replacing it with a connection this process owns
and drives from inside the response body ([src/upstream.rs](../src/upstream.rs)):

|               | before      | after           |
| ------------- | ----------- | --------------- |
| streaming     | 1,314 req/s | **7,921 req/s** |
| throughput    | 78 MiB/s    | **471 MiB/s**   |
| non-streaming | unchanged   | unchanged       |

A little over **6x**, and it is why response bodies are never parsed: the win
came from deleting a wakeup, not from batching. Coalescing already-arrived
frames was measured at a merge ratio of exactly 1.000 in three separate
settings — there is never a second frame waiting.

### How much is left

A dumb bidirectional TCP relay — parsing nothing, framing nothing, the hard
floor for any proxy — was measured on the same path:

|                      | throughput    |
| -------------------- | ------------- |
| direct hop, no proxy | 951 MiB/s     |
| dumb TCP relay       | 694 MiB/s     |
| **fastllm-proxy**    | **524 MiB/s** |

So the entire remaining prize over a proxy that understands nothing is
**1.32x**, and only if detecting end-of-response were free — it is not, since
the in-flight guard has to be released, the connection returned to the pool,
and the next request served on the client socket, all of which need exactly the
framing such a relay would skip. Paying for that with a hijacked client socket
and no pooling is a bad trade.

### Measured and rejected — do not retry without new evidence

Kept with their numbers so nobody re-litigates them from intuition. Nothing is
currently identified as worth doing.

- **Coalescing already-arrived frames.** Merge ratio measured at exactly 1.000
  in three separate settings: against the pooled client, against the owned
  connection that replaced it, and against a real vLLM. There is never a second
  frame waiting. It was the deleted wakeup, not batching, that gave the 6x.
- **A hand-rolled `model` scanner to skip the JSON parse.** 67.1k → 67.2k req/s
  on 64 KiB bodies. `serde_json` skips what it does not want at ~16 B/ns and the
  parse is ~3% of a request; a bespoke parser on the routing path is not worth
  the risk of misrouting.
- **Pre-parsed `Uri` per backend per endpoint.** ~0.2%, and it forces an
  endpoint-index coupling between `proxy.rs` and `registry.rs`.
- **Anything else on the request path.** There is under 1µs available in total.
- **`rewrite_model_if_needed` is not a JSON round trip in the common case.**
  Raised again in review as "a full re-serialize on every rewrite"; it returns
  the body unchanged when the names match, and splices the bytes in place when
  the `model` field's range is known. The `serde_json::Value` path is the
  fallback for a body neither applies to.

### Usage extraction was the most expensive thing on the response path

Found while benchmarking telemetry, and unrelated to it. `TailBuffer::extract_usage`
runs once per request for any principal with a budget or a token rate limit. It
parsed **every** `data:` line in the 8 KiB tail into a full `serde_json::Value`
— around sixty allocated trees — to find the usage chunk, which sits at the
very end.

|                      | before  | after  |
| -------------------- | ------- | ------ |
| usage present        | 22.0 µs | 0.6 µs |
| no usage in the tail | 32.4 µs | 2.8 µs |

Against roughly 38 µs of core time per request, the old figures were most of a
request again. Two changes: search backwards and stop at the first hit, since
"the last matching line wins" is the same answer as "the first match from the
end"; and scan for a single byte before comparing, which the compiler
vectorises, rather than `windows(n)` comparing at every offset.

### What telemetry costs

Measured on this machine with `bench/micro`, because "no performance impact" is
a claim and claims here need numbers.

| instrument                           | cost  |
| ------------------------------------ | ----- |
| `Instant::now()`                     | 14 ns |
| `Instant::elapsed()` to microseconds | 19 ns |
| `AtomicU64` increment, uncontended   | 2 ns  |
| `Histogram::record_us`, uncontended  | 2 ns  |
| `Histogram::record_us`, 2 threads    | 29 ns |
| `Histogram::record_us`, 8 threads    | 57 ns |

A request pays one clock read on arrival, one per-model lookup, and on
completion one elapsed plus two histogram records and a couple of counter
increments — **roughly 80-150 ns depending on contention, against ~38 µs of
core time per request.** Under half a percent, and the per-request fixed work
in the table above is unchanged.

Two design choices carry most of that. The histogram has no `count` field —
the total is the sum of its buckets, added up at scrape time — because a
third atomic on a single cache line cost more than it sounds: 91 ns per record
at 8 threads with it, 57 ns without. And nothing formats a string while
serving; labels are resolved when the snapshot is built, and the only
allocation is on `/metrics`, at scrape time.

### Classifier tiers

Semantic routing costs what it measures. Same machine, `--release` — see
[what the classifier costs](classifier/measurements.md) for the data and the
method:

| tier | model                 | p50 per prompt | what it separates             |
| ---- | --------------------- | -------------- | ----------------------------- |
| 1    | potion-base-8M        | 103 µs         | subject-matter classes        |
| 1    | **potion-code-16M**   | **115 µs**     | best on coding (98.7%)        |
| 1    | potion-retrieval-32M  | 137 µs         | best all-round (90.0%)        |
| 2    | all-MiniLM-L6-v2      | 1.66 ms        | modest gain over tier 1       |
| 2    | **bge-small-en-v1.5** | **3.27 ms**    | same-subject/different-intent |

Tier 1 is a token-vector lookup and a mean — no transformer, no matmul. Cost
also _plateaus_ rather than growing with the prompt, because the encoder stops
at its token cap: a 64 KB paste costs exactly what a 4 KB one does.

Measured accuracy, held out over ~21k human-labelled prompts
(`HuggingFaceH4/no_robots`, `openai/gsm8k`, eleven StackExchange communities):

| class      | tier 1 precision | recall |
| ---------- | ---------------- | ------ |
| coding     | 97.6%            | 92.6%  |
| chat       | 95.8%            | 98.0%  |
| generation | 96.8%            | 69.6%  |
| math       | 88.0%            | 97.6%  |
| devops     | 86.8%            | 90.5%  |
| finance    | 86.2%            | 91.6%  |
| legal      | 85.9%            | 75.0%  |
| security   | 84.8%            | 82.3%  |
| factual-qa | 83.7%            | 50.4%  |
| databases  | 82.3%            | 91.1%  |

Tier 2 is consulted **only** when a routing rule names a class that needs it.
If no rule does, the transformer is never loaded and no request can pay for it.
On realistic traffic mixes escalation touches under a tenth of requests, which
puts the average added cost near 0.2 ms.

Two findings worth knowing before configuring classes:

- **Classify by subject, not by verb.** Subject-matter classes (legal, finance,
  security, coding) reach 82-98% precision on tier 1. Task-shaped classes
  (summarise, rewrite, extract) fail on _both_ tiers — under bge-small,
  Summarize scores 46.6% and Extract 35.6%, worse than tier 1. Telling
  "summarise this" from "extract the dates" needs instruction understanding,
  not better embeddings.
- **Margins are not comparable across models.** bge-small reports higher raw
  cosine similarities than the static model while classifying better, because
  its space is anisotropic. Confidence floors are calibrated per class _and_
  per tier.

### Against LiteLLM

Measured 2026-08-07 on the kw cluster. Both gateways: **one replica, 4 CPU /
6 GiB limits, pinned to the same otherwise-idle 8-core arm64 node, reached over
NodePort** (not a kube-vip LoadBalancer VIP, so the VIP's L2 path is not in the
measurement). Same two vLLM backends, same model, same prompts, same load
generator on the same LAN. LiteLLM runs 4 uvicorn workers in `PRODUCTION` mode
with callbacks and caching off — anything that would slow it down without being
intrinsic to it is turned off, because a comparison that misconfigures the
other side proves nothing. Manifests are in [bench/compare/](../bench/compare/).

**With a real GPU in the path**, 7-8 interleaved A/B pairs per concurrency
level. Ratios are computed _within_ each pair, because absolute throughput on a
shared GPU drifts between sessions and only the paired comparison cancels it.
Backend attribution was verified from each vLLM replica's
`vllm:request_success_total`: both gateways spread across both replicas
(fastllm-proxy 13/14, LiteLLM 15/11 over one run), so neither was accidentally
running against half the hardware.

|                       | fastllm-proxy        | LiteLLM          | median ratio |
| --------------------- | -------------------- | ---------------- | ------------ |
| TTFT p50, 4 streams   | **161 ms** (134-277) | 189 ms (160-544) | **1.28x**    |
| TTFT p50, 8 streams   | **173 ms** (165-184) | 201 ms (186-469) | **1.14x**    |
| throughput, 4 streams | 74 tok/s             | 69 tok/s         | 1.09x        |
| throughput, 8 streams | 133 tok/s            | 122 tok/s        | 1.03x        |

**Throughput is close to a wash.** At 8 concurrent streams the median advantage
is 3%, which is inside the run-to-run noise of a shared GPU. At concurrency 1
(not tabled) it is a tie, and LiteLLM won some rounds. If your bottleneck is the
GPU, the gateway barely moves your token rate — that is the honest headline for
a single-GPU deployment.

**Latency and its consistency are where the difference is real.** Median TTFT is
14-28% lower, and the spread matters more than the median: across eight rounds
at 8 concurrent streams, fastllm-proxy's TTFT stayed within **165-184 ms** while
LiteLLM's ranged **186-469 ms**. A p50 that moves by 20 ms and a p50 that moves
by 280 ms are different products even when their medians are close.

**With the GPU removed from the path** — a mock upstream that answers instantly,
so the gateway is the only thing left to measure:

| concurrency | TTFT p50, fastllm-proxy | TTFT p50, LiteLLM | ratio |
| ----------- | ----------------------- | ----------------- | ----- |
| 1           | **9.7 ms**              | 70.4 ms           | 7.3x  |
| 8           | **14.3 ms**             | 224.8 ms          | 15.7x |
| 32          | **37.4 ms**             | 705.5 ms          | 18.9x |

This is what the gateway itself costs, and it is where the architecture shows:
the gap _widens_ with concurrency rather than narrowing. It also sets the
ceiling on what the choice can ever be worth to you — you only get it back when
the GPU is not the bottleneck, which means many backends, short generations, or
high concurrency.

Two caveats, both against our own favour:

- **The mock throughput numbers are not usable and are not quoted.** Under the
  mock's instant-burst framing LiteLLM delivered 101 SSE events and 600
  characters where fastllm-proxy delivered 199 and 1194 — roughly half the
  payload. Against the real vLLM both delivered 38 events and matching content,
  so this is an artefact of the mock's pacing rather than something LiteLLM does
  in production. Only TTFT is quoted from that run, and only as indicative.
- **kube-proxy and two LAN hops are inside both sets of numbers.** They inflate
  both sides equally, which compresses the ratios — so the pure gateway
  difference is larger than what is reported here, not smaller.

### What has not been measured

LiteLLM is the only other gateway measured; Envoy, Kong, Portkey and the rest
are not. And every number here comes from one cluster, on arm64, on one day.
Re-measure on your own hardware before betting on any of it.

Reproduce any of this with `cargo run -p bench --release --bin realbench`
(and siblings) — see [bench/](../bench/).

## Usage accounting on every request

**Measured** on a kw worker node (`worker-25`, aarch64, 8 cores), release
build, `cargo run -p bench --release --bin tailparse`.

Usage recording used to be limited to principals with a budget or a
tokens-per-minute limit, so this cost fell on a minority of traffic. It is
now paid on every request that has a principal, which makes it a per-request
cost this file owes a number for.

| what                                             | per request | when                |
| ------------------------------------------------ | ----------- | ------------------- |
| `TailBuffer::push`, one SSE frame                | **68 ns**   | per frame forwarded |
| `TailBuffer::push`, a 60-frame stream            | **661 ns**  | whole stream        |
| `extract_usage`, small non-streaming body        | **2.55 µs** | once, at end        |
| `extract_usage`, SSE tail (60 frames)            | **1.33 µs** | once, at end        |
| `extract_usage`, 22 KB body (tail is a fragment) | **8.40 µs** | once, at end        |
| `extract_usage`, tail carrying no usage          | **0.21 µs** | once, at end        |

Against a request whose core proxy cost is ~38 µs (`bench/micro`), the
common cases add roughly 3–7%. The expensive row is the one the tail-buffer
fix added: a body far larger than the 8 KiB window, where the tail is a
fragment and the backwards scan walks it before finding the `usage` key. It
is paid by embeddings and by long non-streaming completions, and it buys
token counts that were previously dropped on the floor — 8 µs against a
request that took 22 ms upstream.

**None of this is I/O.** `record` is a non-blocking `try_send` into a bounded
queue drained by a background flush, so `tests/no_io_on_hot_path.rs` still
holds. The measurement is here because "one small parse per request" was an
adjective until it had a number.

Not tried, and why: moving the parse off the request thread entirely. It
would trade 1–8 µs of latency for a second copy of the tail per request,
which is the wrong side of the trade at these magnitudes — and the parse is
already the last thing that happens on a body that has finished streaming,
so the client is not waiting on it.
