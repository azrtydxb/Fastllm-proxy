# fastllm-proxy

A low-latency OpenAI-compatible gateway for multi-node LLM serving, written in Rust.

It fronts several inference backends (vLLM, SGLang, llama.cpp — anything speaking the OpenAI API) behind one endpoint, and it is built for the case where a general-purpose gateway costs you throughput instead of adding it.

It reads LiteLLM-format config files unchanged, so it drops into an existing `sparkrun proxy` setup without rewriting anything.

## Why

Two separate things go wrong when a conventional gateway sits in front of prefix-caching inference engines.

**Round-robin destroys the prefix cache.** vLLM and SGLang keep a radix/prefix KV cache. Two requests sharing a system prompt are far cheaper on the *same* node — the second reuses the first's cached prefix instead of prefilling it again. A round-robin balancer alternates them by construction, so every request pays full prefill. Nodes look evenly loaded while aggregate throughput drops, and adding a second node can make things *worse* than one.

**Per-token work in the proxy is per-token overhead.** A gateway that deserialises each SSE chunk to re-emit it is doing thousands of parse/re-encode cycles per second per stream. That is latency added to every token.

fastllm-proxy addresses both: routing is prefix-aware, and response bodies are never parsed.

## What it does

- **Cache-affinity routing** with a load escape hatch. A shared prefix goes back to the node that already has its KV cache, unless that node is meaningfully hotter than the least-loaded one.
- **Opaque response bodies.** Upstream frames reach the client exactly as they arrived — never deserialised, never re-encoded, never buffered.
- **Cache-affinity routing, virtual models, rule-based and semantic routing** — see below.
- **20 providers, and any OpenAI-compatible endpoint** — see below.
- **RBAC with real API keys.** Per-principal, per-model grants; keys hashed with SHA-256, passwords with Argon2id.
- **Rate limits, token budgets and usage accounting**, enforced without a database call on the request path.
- **Control plane / data plane split** by a runtime flag, so one image is a single container in a lab and a scaled deployment in Kubernetes.
- **Reloads in place.** `SIGHUP` or a snapshot poll swaps the routing table atomically; in-flight generations are unaffected.

It reads LiteLLM-format config files unchanged, so it drops into an existing setup without rewriting anything.

## Providers

**Twenty providers work today, and adding one is a row in a table — not a code change, not a release.** Anything speaking the OpenAI API is already supported, whether or not it is on this list.

| reached as-is (OpenAI-compatible) | |
|---|---|
| **OpenRouter** (fronts ~400 models) | `https://openrouter.ai/api/v1` |
| OpenAI · Groq · DeepSeek · xAI | `api.openai.com` · `api.groq.com` · `api.deepseek.com` · `api.x.ai` |
| Together · Fireworks · Nebius · AtlasCloud | four endpoints, four rows |
| Z.ai · BigModel · Aliyun DashScope · Qwen Cloud | |
| Moonshot / Kimi · Baidu Qianfan · AIHubMix | |
| GitHub Models · Ollama · vLLM · SGLang · llama.cpp | self-hosted or hosted, same row shape |

| reached through their own wire format | |
|---|---|
| **Anthropic** | `"protocol": "anthropic"` — Messages API, `x-api-key`, SSE re-framed to OpenAI chunks |
| **Gemini** | `"protocol": "gemini"` — `generateContent`, model in the URL, `x-goog-api-key` |

Native translation is opt-in per backend and byte-exact passthrough is preserved everywhere else — an `openai` backend's response is never parsed, which is where the latency numbers above come from. Full endpoint table and the translation limits are in [docs/api.md](docs/api.md#providers).

## Routing

A **virtual model** is a client-facing name with an ordered list of rules and a fallback chain. First rule whose conditions match wins; conditions within a rule are AND'd; targets are weighted *and* ordered, so a rule is both a split and a failover chain.

Route on any of these:

| condition | what it matches |
|---|---|
| `principals`, `roles` | who is calling |
| `min/max_prompt_tokens` | how large the prompt is |
| `min/max_max_tokens` | how much generation was asked for |
| `stream` | whether a human is waiting |
| `headers` | exact header values — the client labels its own workload |
| `min/max_budget_used_percent` | how much of the caller's budget is spent |
| `max_inflight_per_backend` | how busy this rule's own targets are |
| `after`, `before`, `days`, `utc_offset_minutes` | wall-clock window, wrapping midnight |

```jsonc
// Local while the GPUs have room, cloud when they do not. No separate
// "spill" mechanism — first-match-wins does the work.
[{"position": 0, "max_inflight_per_backend": 2, "targets": ["local-qwen"]},
 {"position": 1,                                "targets": ["openrouter"]}]

// Batch work, labelled by the client, goes somewhere cheaper.
{"position": 0, "headers": {"x-fastllm-tier": "batch"}, "targets": ["cheap"]}

// Past 80% of budget, degrade to the free local model instead of 402-ing.
{"position": 0, "min_budget_used_percent": 80, "targets": ["local-qwen"]}
```

**Failover is part of routing, not a separate retry layer.** A rule's targets are tried in order on `5xx`, on `429`, and on an unreachable upstream — before any byte reaches the client. `429` counts because a hosted provider refusing a request is not the same as being unhealthy. Failover never widens reach: a candidate the caller lacks a grant on is dropped from the chain. Details in [docs/api.md](docs/api.md#routing-rules).

### Semantic routing

Route on **what a prompt is about**. A class is a name plus example prompts — no training step, no model to fine-tune, no labelled corpus. The control plane averages the examples into a centroid; the request path embeds the prompt and takes the nearest one.

```bash
curl -X POST https://control/admin/prompt-classes -b "$SESSION" \
  -d '{"name":"coding","examples":["Why does this Rust code fail the borrow checker?", "..."]}'
```
```jsonc
{"position": 0, "class": "coding", "targets": ["claude-sonnet"]}
```

**Two tiers, and you only pay for the second when you ask for it.**

| | model | cost | separates |
|---|---|---|---|
| fast | static embedding | **115 µs** | subject matter — coding, maths, chat, legal, finance, security, databases, devops |
| refined | transformer | 3.3 ms | same subject, different intent — architecture vs. debugging |

The refined tier is gated on configuration, not a flag: if no rule names a class that needs it, the transformer is never loaded and no request can pay for it. When it is enabled, only requests the fast tier landed on a competing class escalate — under a tenth of traffic in practice, putting the average added cost near 0.2 ms.

Measured over ~21k human-labelled prompts, the fast tier reaches **82–98% precision** on twelve classes. Below a per-class confidence floor a rule simply does not match and the next rule catches the request, which is a routing decision rather than an error.

Two findings worth knowing before defining classes — **classify by subject, not by verb** (summarise / rewrite / extract fail on *both* tiers), and check `POST /admin/prompt-classes/evaluate`, which reports which of your classes collide. Full per-class numbers and the benchmark harness: [docs/classifier.md](docs/classifier.md).

## How it compares

Measured 2026-08-07 against **LiteLLM** on an arm64 Kubernetes cluster. Both gateways: one replica, 4 CPU / 6 GiB, same idle node, reached over NodePort, same two vLLM backends, interleaved A/B runs. LiteLLM ran 4 uvicorn workers in `PRODUCTION` mode. Manifests: [bench/compare/](bench/compare/).

### With the GPU removed — what the gateway itself costs

A mock upstream that answers instantly, so the gateway is the only thing being measured.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-mock-throughput-dark.svg"><img alt="Requests per second against a mock upstream. fastllm-proxy climbs to roughly 500-635 per second; LiteLLM plateaus near 36." src="docs/images/bench-mock-throughput-light.svg" width="49%"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-mock-latency-dark.svg"><img alt="Median time to first token against a mock upstream, log scale. fastllm-proxy stays between 8 and 46 milliseconds; LiteLLM rises from 87 to 1313." src="docs/images/bench-mock-latency-light.svg" width="49%"></picture>

**~15x the throughput and 10-28x lower latency**, and the gap widens with concurrency rather than narrowing. This is the ceiling on what the choice can be worth — you collect it only when the GPU is not your bottleneck.

### With real GPUs — throughput is a wash, consistency is not

Two vLLM replicas, 16 concurrent slots each. A gateway that balances correctly should climb to 32 concurrent streams and then flatten. Both do, and land on the same ceiling.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-real-throughput-dark.svg"><img alt="Aggregate tokens per second against two real vLLM replicas. Both gateways climb together and flatten at about 305 to 332 tokens per second from 32 concurrent streams onward." src="docs/images/bench-real-throughput-light.svg" width="49%"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-real-latency-dark.svg"><img alt="Time to first token against real vLLM, p50 solid and p99 dotted. Medians track closely; the 99th percentile diverges sharply at 32 streams." src="docs/images/bench-real-latency-light.svg" width="49%"></picture>

**Aggregate throughput is a wash** — both saturate the same GPUs, and at a single stream LiteLLM won several rounds outright. If your bottleneck is the GPU, the gateway barely moves your token rate.

What does differ is steadiness. At 32 streams, p99 time-to-first-token is **766 ms against 2921 ms**, and the gap between consecutive tokens is 15-25% less variable at *every* concurrency level:

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-real-jitter-dark.svg"><img alt="Standard deviation of the gap between tokens against real vLLM. fastllm-proxy is consistently 15 to 25 percent lower than LiteLLM at every concurrency level." src="docs/images/bench-real-jitter-light.svg" width="49%"></picture>

A p50 that moves by 20 ms and one that moves by 280 ms are different products even when their medians match.

Full conditions, the per-request cost breakdown, the synthetic ceilings and what has *not* been measured are in [docs/performance.md](docs/performance.md).

## Install

```bash
cargo build --release
# target/release/fastllm-proxy
```

## Documentation

| | |
|---|---|
| [Architecture](docs/architecture.md) | Component and request-flow diagrams, failure modes, consistency guarantees, behaviour notes |
| [Performance](docs/performance.md) | Every measured number, its conditions, and what has not been measured |
| [Semantic routing](docs/classifier.md) | Classifier tiers: measured accuracy, cost, and what is still to build |
| [Running it](docs/operations.md) | Install, the three roles, deployment shapes, configuration reference |
| [API and administration](docs/api.md) | Endpoints, admin API, providers, routing rules, auth, TLS, budgets, rate limits |
| [Deployment on Kubernetes](deploy/README.md) | Manifests, adding a provider, operator runbook |
| [TODO](TODO.md) | What was tried and rejected, with numbers |

## Development

```bash
cargo test
cargo build --release
```

## License

Apache-2.0
