# fastllm-proxy

A low-latency OpenAI-compatible gateway for multi-node LLM serving, written in
Rust. It fronts several inference backends — vLLM, SGLang, llama.cpp, anything
speaking the OpenAI API — behind one endpoint, and it is built for the case
where a general-purpose gateway costs you throughput instead of adding it.

Two things go wrong when a conventional gateway sits in front of prefix-caching
engines, and both are addressed here rather than tuned around:

**Round-robin destroys the prefix cache.** Two requests sharing a system prompt
are far cheaper on the *same* node, because the second reuses the first's cached
prefix instead of prefilling it again. A round-robin balancer alternates them by
construction. Routing here is prefix-aware, with a load escape hatch.

**Per-token work in the proxy is per-token overhead.** A gateway that
deserialises each SSE chunk to re-emit it does thousands of parse/re-encode
cycles per second per stream. Response bodies here are never parsed — an
`openai` backend's frames reach the client exactly as they arrived.

## Where to start

| | |
|---|---|
| [Quickstart and clients](integrations.md) | Point an SDK, a coding agent or a framework at it |
| [Troubleshooting](troubleshooting.md) | The failures people actually hit |
| [Operations](operations.md) | The three roles, configuration, metrics, logs |
| [API and administration](api.md) | Every endpoint, and `openapi.json` |
| [Architecture](architecture.md) | Diagrams, failure modes, consistency guarantees |
| [Performance](performance.md) | Every measured number and its conditions |

Deploying to Kubernetes: the [Helm chart](https://github.com/azrtydxb/Fastllm-proxy/tree/main/charts/fastllm-proxy),
or the [worked manifests](https://github.com/azrtydxb/Fastllm-proxy/tree/main/deploy)
for one real cluster.

## Already running LiteLLM?

```bash
fastllm-proxy import --config litellm_config.yaml --database-url postgres://...
```

Models, backends, keys and each key's per-model grants come across. It is
idempotent, and re-importing an edited file converges rather than duplicating.
`File` mode reads LiteLLM YAML directly if you would rather not adopt the
database at all.
