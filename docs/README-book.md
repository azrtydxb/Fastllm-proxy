# fastllm-proxy

**One OpenAI-compatible endpoint in front of every model you run — that does
not cost you the throughput it was supposed to add.**

A gateway written in Rust for teams running their own inference on more than
one node. It routes on prefix affinity so your KV cache survives load
balancing, never parses a response body it is only forwarding, and enforces
RBAC, rate limits and budgets without a database call on the request path.

```bash
docker run ghcr.io/azrtydxb/fastllm-proxy:v0.1.0 --help
```

[**Get started →**](getting-started.md) · [What it can do →](features.md) · [Connect a client →](integrations.md)

---

![The management UI: request rate, backends up, error rate and in-flight, a 24-hour traffic chart read from the database, per-backend health per replica, and the audit log's head](images/ui-overview.png)

Thirteen screens, embedded in the binary. No separate service, no extra
deployment.

---

## The problem it solves

Two things go wrong when a general-purpose gateway sits in front of
prefix-caching engines like vLLM or SGLang. Both are addressed structurally
here rather than tuned around.

**Round-robin destroys the prefix cache.** These engines keep a radix/prefix
KV cache, so two requests sharing a system prompt are far cheaper on the
*same* node — the second reuses the first's cached prefix instead of
prefilling it again. A round-robin balancer alternates them by construction.
Nodes look evenly loaded while aggregate throughput drops, and adding a second
node can make things **worse** than one.

**Per-token work in the proxy is per-token overhead.** A gateway that
deserialises each SSE chunk to re-emit it does thousands of parse/re-encode
cycles per second per stream. That is latency added to every token, and it
grows with how much your users read.

## What that is worth

Measured against LiteLLM on the same cluster, same backends, interleaved A/B
runs. With the GPU removed, so the gateway is the only thing being measured:

<picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-mock-throughput-dark.svg"><img alt="Requests per second against a mock upstream. fastllm-proxy climbs to roughly 500-635 per second; LiteLLM plateaus near 36." src="images/bench-mock-throughput-light.svg" width="49%"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-mock-latency-dark.svg"><img alt="Median time to first token against a mock upstream, log scale. fastllm-proxy stays between 8 and 46 milliseconds; LiteLLM rises from 87 to 1313." src="images/bench-mock-latency-light.svg" width="49%"></picture>

**~15× the throughput, 10–28× lower latency** — and now the part most
comparisons leave out.

**With real GPUs, aggregate throughput is a wash.** Both gateways saturate the
same hardware and land on the same ceiling; at a single stream LiteLLM won
several rounds outright. The figures above are a *ceiling* on what a gateway
can be worth, collected only when the GPU is not your bottleneck.

What survives contact with real hardware is **steadiness**:

<picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-real-jitter-dark.svg"><img alt="Standard deviation of the gap between tokens against real vLLM. fastllm-proxy is consistently 15 to 25 percent lower than LiteLLM at every concurrency level." src="images/bench-real-jitter-light.svg" width="60%"></picture>

At 32 concurrent streams, p99 time-to-first-token is **766 ms against
2921 ms**, and the gap between consecutive tokens is 15–25% less variable at
*every* concurrency level. A p50 that moves by 20 ms and one that moves by
280 ms are different products even when their medians match.

Against a real vLLM the proxy's own overhead is **below the noise floor** —
0.76 µs of per-request work against ~38 µs of core cost.

[Every number, its conditions, and what was *not* measured →](performance.md)

## What you get

| | |
|---|---|
| **Cache-affinity routing** | A shared prefix returns to the node holding its KV cache, unless that node is meaningfully hotter than the least-loaded one. `least-loaded`, `round-robin` and `lowest-latency` are selectable |
| **Virtual models** | One client-facing name, ordered rules, weighted *and* ordered targets — so a rule is both a traffic split and a failover chain |
| **Rule-based routing** | Match on principal, role, prompt size, requested generation, streaming, headers, budget consumption, in-flight count or time of day |
| **Semantic routing** | A ~115 µs static-embedding tier decides most prompts; a transformer loads only if a rule asks for one |
| **RBAC with real keys** | Principals, roles, per-model grants. Keys SHA-256 hashed, passwords Argon2id — deliberately different |
| **Budgets and rate limits** | Enforced with an integer comparison on the request path, not a query |
| **Usage accounting** | Every attributable request, priced at the price in force when it ran, in integer micro-units |
| **80 providers** | Anything OpenAI-shaped is a row in a table. Anthropic and Gemini in their own wire format, translated both ways including streaming and tool calls |
| **Control/data plane split** | One binary, three shapes. A proxy that loses its control plane keeps serving from its last snapshot |
| **Thirteen-screen UI** | Embedded in the binary, with history, drill-downs and an audit log |

## Routing you can inspect before you trust it

![The Virtual models screen, showing a rule's conditions and weighted targets alongside a dry-run panel](images/ui-virtual-models.png)

**Dry-run** answers which rule would decide and what the chain resolves to,
without dispatching anything. Because a routing table you cannot interrogate
is a routing table you find out about in production.

## History, not just a live view

![The traffic drill-down: 1h to 30d ranges, pan controls, filters by model and principal, and stacked charts for requests, latency and tokens](images/ui-timeseries-modal.png)

Requests stacked as served / upstream errors / refusals-by-kind — because a
caller stopped by a budget and a backend that fell over need different people
to do different things. A gap in the latency line is a bucket with nothing to
measure, never zero.

## Already running LiteLLM?

```bash
fastllm-proxy import --config litellm_config.yaml --database-url postgres://...
```

Models, backends, keys and each key's per-model grants come across. Idempotent
— re-importing an edited file converges rather than duplicating, and grants
removed from the file are revoked. Your existing keys keep working against the
same models they already had.

`File` mode reads LiteLLM YAML directly, if you would rather not adopt the
database at all.

## More than chat

Twelve endpoints, not one — and each is the same one-row configuration,
authorised by the same per-model grants and counted in the same usage
accounting:

| | |
|---|---|
| **Chat & completions** | `/chat/completions`, `/completions`, `/responses` |
| **Images** | `/images/generations`, `/images/edits` |
| **Speech** | `/audio/speech`, `/audio/transcriptions`, `/audio/translations` |
| **Embeddings & ranking** | `/embeddings`, `/rerank`, `/score` |
| **Safety** | `/moderations` |

Multipart audio uploads are forwarded without their boundary being touched;
binary responses go through the same byte pump as a token stream.

## On the roadmap

Guardrails and PII masking · SSO/SAML · teams and organisations ·
native wire-format translators for providers that do not speak OpenAI's shape ·
usage-based routing.

[What each of those involves →](features.md#on-the-roadmap)

## Start here

| | |
|---|---|
| [Getting started](getting-started.md) | Install, first request, and a tour of every screen |
| [What it can do](features.md) | Features, measured trade-offs, honest limits |
| [Connecting a client](integrations.md) | OpenAI SDKs, five coding agents, four frameworks |
| [Troubleshooting](troubleshooting.md) | The failures people actually hit |
| [Operations](operations.md) | The three roles, deployment shapes, configuration |
| [API and administration](api.md) | Every endpoint, plus `openapi.json` and Swagger |
| [Architecture](architecture.md) | How the pieces fit, and how they fail |

Deploying to Kubernetes: the [Helm chart](https://github.com/azrtydxb/Fastllm-proxy/tree/main/charts/fastllm-proxy),
or the [worked manifests](https://github.com/azrtydxb/Fastllm-proxy/tree/main/deploy)
for one real cluster.

Apache-2.0 · [source](https://github.com/azrtydxb/Fastllm-proxy) · [v0.1.0](https://github.com/azrtydxb/Fastllm-proxy/releases/tag/v0.1.0)
