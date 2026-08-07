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
- **Every OpenAI-compatible provider** as configuration, not code: OpenRouter, Groq, DeepSeek, xAI, Together, Fireworks and the rest are a backend row. Anthropic and Gemini are reached through their native APIs by setting one field. See [routing and providers](docs/api.md#providers).
- **RBAC with real API keys.** Per-principal, per-model grants; keys hashed with SHA-256, passwords with Argon2id.
- **Rate limits, token budgets and usage accounting**, enforced without a database call on the request path.
- **Virtual models and routing rules** — route on caller, prompt size, headers, streaming, budget consumed, backend load or time of day, with an ordered fallback chain across models. See [routing rules](docs/api.md#routing-rules).
- **Control plane / data plane split** by a runtime flag, so one image is a single container in a lab and a scaled deployment in Kubernetes.
- **Reloads in place.** `SIGHUP` or a snapshot poll swaps the routing table atomically; in-flight generations are unaffected.

It reads LiteLLM-format config files unchanged, so it drops into an existing setup without rewriting anything.

## How it compares

Measured 2026-08-07 against **LiteLLM** on an arm64 Kubernetes cluster. Both gateways: one replica, 4 CPU / 6 GiB, same idle node, reached over NodePort, same two vLLM backends, interleaved A/B runs. LiteLLM ran 4 uvicorn workers in `PRODUCTION` mode. Manifests: [bench/compare/](bench/compare/).

### With the GPU removed — what the gateway itself costs

A mock upstream that answers instantly, so the gateway is the only thing being measured.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-mock-throughput-dark.svg"><img alt="Requests per second against a mock upstream. fastllm-proxy climbs to roughly 500-635 per second; LiteLLM plateaus near 36." src="docs/images/bench-mock-throughput-light.svg" width="49%"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-mock-latency-dark.svg"><img alt="Median time to first token against a mock upstream, log scale. fastllm-proxy stays between 8 and 46 milliseconds; LiteLLM rises from 87 to 1313." src="docs/images/bench-mock-latency-light.svg" width="49%"></picture>

**~15x the throughput and 10-28x lower latency**, and the gap widens with concurrency rather than narrowing. This is the ceiling on what the choice can be worth — you collect it only when the GPU is not your bottleneck.

### With real GPUs — throughput is a wash, consistency is not

Two vLLM replicas, 16 concurrent slots each. A gateway that balances correctly should climb to 32 concurrent streams and then flatten. Both do, and land on the same ceiling.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-real-throughput-dark.svg"><img alt="Aggregate tokens per second against two real vLLM replicas. Both gateways climb together and flatten at about 305 to 332 tokens per second from 32 concurrent streams onward." src="docs/images/bench-real-throughput-light.svg" width="49%"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-real-latency-dark.svg"><img alt="Time to first token against real vLLM, p50 solid and p99 dotted. Medians track closely; the 99th percentile diverges sharply at 32 streams." src="docs/images/bench-real-latency-light.svg" width="49%"></picture>

**Aggregate throughput is a wash** — both saturate the same GPUs, and at a single stream LiteLLM won several rounds outright. If your bottleneck is the GPU, the gateway barely moves your token rate. Say so plainly rather than quoting the mock number and hoping nobody asks.

What does differ is steadiness. At 32 streams, p99 time-to-first-token is **766 ms against 2921 ms**, and the gap between consecutive tokens is 15-25% less variable at *every* concurrency level:

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/bench-real-jitter-dark.svg"><img alt="Standard deviation of the gap between tokens against real vLLM. fastllm-proxy is consistently 15 to 25 percent lower than LiteLLM at every concurrency level." src="docs/images/bench-real-jitter-light.svg" width="49%"></picture>

A p50 that moves by 20 ms and one that moves by 280 ms are different products even when their medians match.

**Caveats, on the record:** the mock exposed a framing difference we deliberately do not quote as a win; kube-proxy and two LAN hops sit inside both sides and compress the ratios; and this is one cluster on one day. Full conditions, the per-request cost breakdown, the synthetic ceilings and what has *not* been measured are in [docs/performance.md](docs/performance.md).

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
