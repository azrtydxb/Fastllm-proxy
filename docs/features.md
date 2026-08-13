# What it does, and when it is the right choice

There are several good LLM gateways. This one exists for a specific case, and
it is worth saying plainly where it wins, where it draws, and where you should
use something else — a features list that only claims advantages is a sales
page, and you cannot plan a deployment from one.

## The case it is built for

**You run your own inference, on more than one node, and the gateway is
starting to cost you.**

Two things go wrong when a general-purpose gateway sits in front of
prefix-caching engines like vLLM or SGLang:

**Round-robin destroys the prefix cache.** These engines keep a radix/prefix KV
cache. Two requests sharing a system prompt are far cheaper on the *same* node
— the second reuses the first's cached prefix instead of prefilling it again. A
round-robin balancer alternates them by construction, so every request pays
full prefill. Nodes look evenly loaded while aggregate throughput drops, and
adding a second node can make things *worse* than one.

**Per-token work in the proxy is per-token overhead.** A gateway that
deserialises each SSE chunk to re-emit it does thousands of parse/re-encode
cycles per second per stream. That is latency added to every token, and it
scales with how much your users read.

Both are addressed structurally rather than tuned around: routing is
prefix-aware, and an `openai`-protocol response body is never parsed.

## What that is worth, measured

Against LiteLLM, same cluster, same backends, interleaved A/B runs
([full conditions](performance.md)):

| | fastllm-proxy | LiteLLM |
|---|---|---|
| Throughput, mock upstream | **~500–635 req/s** | ~36 req/s |
| TTFT, mock upstream | **8–46 ms** | 87–1313 ms |
| Aggregate tok/s, real GPUs | 305–332 | 305–332 |
| p99 TTFT at 32 streams | **766 ms** | 2921 ms |
| Inter-token jitter | **15–25% lower** | — |

Read the third row before the first two. **With real GPUs, aggregate
throughput is a wash** — both saturate the same hardware, and at a single
stream LiteLLM won several rounds outright. The 15× figure is what the gateway
costs *when the GPU is not your bottleneck*, which is a ceiling on the value,
not a promise of it.

What survives contact with real GPUs is **steadiness**: p99 time-to-first-token
and inter-token jitter. A p50 that moves by 20 ms and one that moves by 280 ms
are different products even when their medians match.

Against a real vLLM the proxy's own overhead is **below the noise floor** —
0.76 µs of per-request work against ~38 µs of core cost, and two runs put it
marginally *ahead* of no-proxy, which is measurement noise and reported as such.

## The features, and why each exists

### Routing that knows about prefixes

Cache-affinity with a load escape hatch: a shared prefix returns to the node
holding its KV cache, unless that node is meaningfully hotter than the
least-loaded one. `least-loaded`, `round-robin` and `lowest-latency` are
selectable — the last for pools whose members are not equivalent, where a slow
backend with one queued request looks emptier than a fast one with two.

### Virtual models: routing as configuration, not code

One client-facing name, ordered rules, weighted *and* ordered targets — so a
rule is both a traffic split and a failover chain. Rules match on principal,
role, prompt size, requested generation, streaming, headers, budget
consumption, in-flight count, time of day, or the prompt's semantic class.

Failover never widens reach: a candidate the caller lacks `model:invoke` on is
dropped from the chain, including the deployment-wide fallback.

### Semantic routing, at a cost you can afford

A ~115 µs static-embedding tier decides most prompts; an int8 ONNX transformer
loads only if a rule names a refined class. Classify by *subject*, not by verb
— the measurements behind that are in [the classifier doc](classifier.md),
including which class pairs collide.

### RBAC that is not a shared secret

Principals, roles, per-model `model:invoke` grants. Keys are SHA-256 hashed;
passwords are Argon2id — deliberately different, because keys are
high-entropy random and passwords are low-entropy and human-chosen.

### Accounting that is enforced without I/O

Rate limits, token budgets and spend, resolved into the snapshot so the
request path does an integer comparison rather than a query. Usage is recorded
for every attributable request, priced at the price in force when the request
ran, and stored in integer micro-units.

### A control plane you can split from the data plane

One binary, three shapes via `--role`. The same image is a single container in
a lab and a scaled deployment in Kubernetes. A proxy that loses its control
plane keeps serving from its last-known-good snapshot rather than failing.

### 80 providers, and adding one is a row in a table

Anything OpenAI-shaped works whether or not it is on the list. Anthropic and
Gemini are reached in their own wire format, translated in both directions
including streaming and tool calls.

## Where it is *not* the right choice

**You want the widest possible provider and modality coverage.** LiteLLM
supports things this deliberately does not: image generation, speech, rerank
providers with bespoke APIs, and a long tail of non-OpenAI-shaped endpoints.
If your gateway needs to be the one integration point for everything, that is
a real advantage and this is not it.

**You need guardrails, PII masking or content filtering in the gateway.** Not
built. There are no hooks for it yet either.

**You need SSO/SAML or multi-tenant teams and org hierarchies.** RBAC here is
principals, roles and per-model grants. There is no team or org layer, and SSO
is [explicitly parked](https://github.com/azrtydxb/Fastllm-proxy/blob/main/TODO.md).

**You want a shared response cache across replicas.** The cache is per process,
and that is a decision rather than an omission: a shared cache means a network
round trip on the read path, which is the one thing the performance story rests
on not doing. The reasoning is written out in `TODO.md`.

**You are on amd64.** Released images are linux/arm64 only, because every node
this is built and tested on is arm64. It is a build-matrix change, not a code
change, but it is not done.

**Your bottleneck is the GPU and you already have a gateway that works.** The
honest reading of the benchmarks above is that you would gain steadier tails
and lose your existing integration. That may not be worth it.

## Where next

| | |
|---|---|
| [Getting started](getting-started.md) | Install, first request, and a tour of the UI |
| [Performance](performance.md) | Every number, its conditions, and what was *not* measured |
| [Architecture](architecture.md) | How the pieces fit and how they fail |
