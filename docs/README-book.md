![FastLLM Proxy](images/logo.webp)

# fastllm-proxy

**The lowest-overhead LLM router. Production-ready, highly available, and one
OpenAI-compatible endpoint in front of everything you serve.**

A load balancer and router written in Rust for teams putting real traffic
through more than one model or node. **0.76 µs of work per request**, no I/O on
the request path at all, and the feature set you would otherwise assemble out
of glue — so it saves you money, uptime, and the numbers you are measured on.

```bash
docker run ghcr.io/azrtydxb/fastllm-proxy:v0.2.0 --help
```

[**Get started →**](getting-started.md) · [Performance →](performance.md) · [Providers →](providers.md) · [Connect a client →](integrations.md)

---

![The management UI: request rate, backends up, error rate and in-flight, a 24-hour traffic chart read from the database, per-backend health per replica, and the audit log's head](images/ui-overview.png)

Sixteen screens, embedded in the binary — seventeen under the Kubernetes
operator, which adds one for the deployment itself. No separate service, no extra
deployment.

---

## What it is

### The lowest overhead of any gateway here

Nothing on the request path does I/O. Not a database call, not a file read —
RBAC, per-model grants, rate limits and budgets are integer comparisons
against a snapshot already flattened in memory, and a test in the repo fails
the build if anything I/O-shaped lands there.

Nothing on the response path parses. An upstream's frames reach your client
exactly as they arrived — never deserialised, never re-encoded, never
buffered. A gateway that decodes each SSE chunk to re-emit it pays thousands
of parse cycles per second per stream; this one pays none, so the cost does
not grow with how much your users read.

And routing knows what your engine knows: **a shared prefix goes back to the
node already holding its KV cache**, unless that node is meaningfully hotter
than the least-loaded one. Round-robin in front of vLLM or SGLang alternates
those requests by construction, so every one pays full prefill — nodes look
evenly loaded while aggregate throughput falls.

### Production-ready, and highly available on purpose

|                                         |                                                                                                                                                      |
| --------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| **It survives its own control plane**   | A proxy that loses the control plane keeps serving from its last-known-good snapshot. Configuration stops changing; traffic does not stop            |
| **Failover is part of routing**         | Ordered targets are tried on `5xx`, on `429`, and on an unreachable upstream — before a byte reaches the client, and never widening a caller's reach |
| **Health is per replica, never merged** | One replica seeing a backend down while others do not is a partition, and averaging deletes the only symptom                                         |
| **Reloads in place**                    | `SIGHUP` or a snapshot poll swaps the routing table atomically. In-flight generations are unaffected                                                 |
| **One binary, three shapes**            | The same image is a single container on a laptop and a scaled deployment in Kubernetes, with a Helm chart and worked manifests                       |

### The most advanced feature set, and none of it on the hot path

80 providers, virtual models with weighted _and_ ordered targets, rule-based
and semantic routing, RBAC with real keys, rate limits, budgets, usage
accounting priced per request, webhooks, Prometheus, OpenTelemetry, an OpenAPI
spec, and a sixteen-screen management UI **embedded in the binary**.

Each of those is a thing you would otherwise stand up, secure, monitor and
carry. Here they are configuration, and none of them costs the request path a
round trip.

## What it saves you

|               |                                                                                                                                                                                                                                 |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Money**     | Prefix affinity keeps your KV cache warm, so you buy throughput from the GPUs you already own rather than from more of them. Budgets and per-model prices make spend a number you can see per principal, not a monthly surprise |
| **Uptime**    | Failover, per-replica health, a control plane you can lose, and reloads that do not drop streams                                                                                                                                |
| **Your KPIs** | p99 time-to-first-token of **766 ms against 2921 ms** at 32 concurrent streams, and inter-token jitter 15–25% lower at _every_ concurrency level. The tail is what your users feel                                              |

## The numbers, and their conditions

Measured against LiteLLM on the same cluster, same backends, interleaved A/B
runs. With the GPU removed, so the gateway is the only thing being measured:

<picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-mock-throughput-dark.svg"><img alt="Requests per second against a mock upstream. fastllm-proxy climbs to roughly 500-635 per second; LiteLLM plateaus near 36." src="images/bench-mock-throughput-light.svg" width="49%"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-mock-latency-dark.svg"><img alt="Median time to first token against a mock upstream, log scale. fastllm-proxy stays between 8 and 46 milliseconds; LiteLLM rises from 87 to 1313." src="images/bench-mock-latency-light.svg" width="49%"></picture>

**~15× the throughput, 10–28× lower latency** — and now the part most
comparisons leave out.

**With real GPUs, aggregate throughput is a wash.** Both gateways saturate the
same hardware and land on the same ceiling; at a single stream LiteLLM won
several rounds outright. The figures above are a _ceiling_ on what a gateway
can be worth, collected only when the GPU is not your bottleneck.

What survives contact with real hardware is **steadiness**:

<picture><source media="(prefers-color-scheme: dark)" srcset="images/bench-real-jitter-dark.svg"><img alt="Standard deviation of the gap between tokens against real vLLM. fastllm-proxy is consistently 15 to 25 percent lower than LiteLLM at every concurrency level." src="images/bench-real-jitter-light.svg" width="60%"></picture>

At 32 concurrent streams, p99 time-to-first-token is **766 ms against
2921 ms**, and the gap between consecutive tokens is 15–25% less variable at
_every_ concurrency level. A p50 that moves by 20 ms and one that moves by
280 ms are different products even when their medians match.

Against a real vLLM the proxy's own overhead is **below the noise floor** —
0.76 µs of per-request work against ~38 µs of core cost.

[Every number, its conditions, and what was _not_ measured →](performance.md)

## What you get

|                              |                                                                                                                                                                                                |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Cache-affinity routing**   | A shared prefix returns to the node holding its KV cache, unless that node is meaningfully hotter than the least-loaded one. `least-loaded`, `round-robin` and `lowest-latency` are selectable |
| **Virtual models**           | One client-facing name, ordered rules, weighted _and_ ordered targets — so a rule is both a traffic split and a failover chain                                                                 |
| **Rule-based routing**       | Match on principal, role, prompt size, requested generation, streaming, headers, budget consumption, in-flight count or time of day                                                            |
| **Semantic routing**         | A ~115 µs static-embedding tier decides most prompts; a transformer loads only if a rule asks for one                                                                                          |
| **RBAC with real keys**      | Principals, roles, per-model grants. Keys SHA-256 hashed, passwords Argon2id — deliberately different                                                                                          |
| **Budgets and rate limits**  | Enforced with an integer comparison on the request path, not a query                                                                                                                           |
| **Usage accounting**         | Every attributable request, priced at the price in force when it ran, in integer micro-units                                                                                                   |
| **A2A gateway**              | Agents behind one address, their cards rewritten so the next call is still authorised, versions pinned rather than guessed                                                                     |
| **MCP gateway**              | Every tool server behind one address, tools namespaced, and `mcp:invoke` grants that are deliberately not implied by `model:invoke`                                                            |
| **80 providers**             | Anything OpenAI-shaped is a row in a table. Anthropic and Gemini in their own wire format, translated both ways including streaming and tool calls                                             |
| **Control/data plane split** | One binary, three shapes. A proxy that loses its control plane keeps serving from its last snapshot                                                                                            |
| **Sixteen-screen UI**        | Embedded in the binary, with history, drill-downs and an audit log — plus a seventeenth under the operator                                                                                     |

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

## More than chat

Twelve endpoints, not one — and each is the same one-row configuration,
authorised by the same per-model grants and counted in the same usage
accounting:

|                          |                                                                 |
| ------------------------ | --------------------------------------------------------------- |
| **Chat & completions**   | `/chat/completions`, `/completions`, `/responses`               |
| **Images**               | `/images/generations`, `/images/edits`                          |
| **Speech**               | `/audio/speech`, `/audio/transcriptions`, `/audio/translations` |
| **Embeddings & ranking** | `/embeddings`, `/rerank`, `/score`                              |
| **Safety**               | `/moderations`                                                  |

Multipart audio uploads are forwarded without their boundary being touched;
binary responses go through the same byte pump as a token stream.

## On the roadmap

Guardrails and PII masking · SSO/SAML · teams and organisations ·
native wire-format translators for providers that do not speak OpenAI's shape ·
usage-based routing.

[What each of those involves →](features.md#on-the-roadmap)

## Start here

|                                        |                                                                              |
| -------------------------------------- | ---------------------------------------------------------------------------- |
| [Getting started](getting-started.md)  | Install, first request, and a tour of every screen                           |
| [Performance](performance.md)          | Every number, its conditions, and what was _not_ measured                    |
| [What it can do](features.md)          | Features, measured trade-offs, honest limits                                 |
| [Providers](providers.md)              | All 80, how to add one, how credentials are handled                          |
| [MCP gateway](mcp.md)                  | One endpoint in front of every tool server, with the same grants             |
| [A2A agents](agents.md)                | One address in front of every agent, card rewritten to keep calls attributed |
| [Connecting a client](integrations.md) | OpenAI SDKs, five coding agents, four frameworks                             |
| [Troubleshooting](troubleshooting.md)  | The failures people actually hit                                             |
| [Operations](operations.md)            | Five deployment shapes, from one binary to a scaled cluster                  |
| [Security](security.md)                | Trust boundaries, what is stored and in what form                            |
| [Command-line reference](cli.md)       | Every flag, every subcommand                                                 |
| [API and administration](api.md)       | Every endpoint, plus `openapi.json` and Swagger                              |
| [Architecture](architecture.md)        | How the pieces fit, and how they fail                                        |
| [Changelog](changelog.md)              | What changed, newest first                                                   |

Deploying to Kubernetes: the [Helm chart](https://github.com/azrtydxb/Fastllm-proxy/tree/main/charts/fastllm-proxy),
or the [worked manifests](https://github.com/azrtydxb/Fastllm-proxy/tree/main/deploy)
for one real cluster.

Apache-2.0 · [source](https://github.com/azrtydxb/Fastllm-proxy) · [v0.2.0](https://github.com/azrtydxb/Fastllm-proxy/releases/tag/v0.2.0)
