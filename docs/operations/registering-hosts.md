# Registering hosts that serve models

A host that serves models can tell FastLLM so, and keep telling it. When it
stops, its providers stop being routed to — and eventually stop existing.

This exists because the registry drifts. Two real cases, both found by hand:
a provider model pointing at a host that had been swapped to serve something
else entirely, and a model with no provider at all. The first is the
interesting one: the host was **healthy and answering**. No liveness check can
catch that, because nothing was down.

## What the agent does

It registers an **address**, on a lease, and refreshes it. That is all.

It deliberately does not send a model list. FastLLM has to reach the endpoint
anyway in order to serve traffic, so the control plane calls `GET /v1/models`
itself: a list pushed from the host could name models the proxies cannot dial,
and that failure would surface at request time, to a user. Enumerating from the
control plane makes discovery and reachability the same test.

It dials the control plane and is never dialled, so it works from a host behind
NAT, or on a cluster FastLLM cannot reach into.

## Running it

```bash
FASTLLM_CONTROL_URL=https://control.example:4001 \
FASTLLM_AGENT_TOKEN=sk-... \
python3 agent/fastllm-node-agent.py \
  --advertise 192.168.10.246 \
  --scan-ports 8000 8001 8890 \
  --engine vllm
```

Standard library only, so there is nothing to install. That is deliberate: this
runs on machines whose Python is whatever the vendor shipped, and a health
agent that needs a virtualenv to start is one more thing to be broken at 3am.

| Flag | Why it matters |
| --- | --- |
| `--advertise` | The address **proxies** will dial. Configured, never inferred — an agent that discovers a container on `172.17.0.2` and registers that hands the proxies an address they cannot reach. |
| `--scan-ports` | Ports to probe on that address. Catches a bare process started by hand or by a launcher, with no container runtime present. |
| `--api-base` | Register an endpoint outright, repeatable. Use when the address is not a port on `--advertise`. |
| `--ttl` / `--interval` | Lease length and heartbeat. The agent refuses an interval that is not well inside the TTL, since one slow beat would then expire the lease. |
| `--engine` | A hint, carried as metadata. Nothing depends on it. |
| `--once` | Register and exit, for a cron or a smoke test. |

## Any engine, in a container or not

Every engine worth naming answers `GET /v1/models` — vLLM, SGLang,
llama.cpp's server, TGI, Ollama, Triton's OpenAI frontend, LM Studio, mlx-lm —
as do the hosted providers. So neither the agent nor the control plane needs to
know which one it found. An unrecognised engine is registered like any other; it
simply contributes no metadata.

There is no container mode. A port probe finds a process however it was started.

## What the control plane does with it

Every `--provider-sweep-interval` (60s by default), one `GET /v1/models` per
provider answers both questions that matter:

- **is it reachable** — the ordinary health question
- **is it still serving what is registered against it** — the drift question,
  which is the one that started all this

A provider serving *more* than is registered is healthy, not drifted. OpenRouter
answers with hundreds of models and three of them are registered; treating the
extras as drift would mark every cloud provider broken.

## Degrade, then delete

A failed probe or a lapsed lease marks the provider **degraded**: its models go
out of rotation, and nothing is deleted. Only after 30 minutes of sustained
absence is a dynamic provider removed.

Two stages, never one. A 27B on a DGX Spark takes over ten minutes to load and
answers nothing while it does, and a host reboot is routine. Deleting on the
first failed probe would make every restart look like a decommissioning — and
deleting a provider throws away its credential. Suppressing routing is
reversible; deletion is not.

> [!IMPORTANT]
> **Static and cloud providers never expire.** They are probed on the same
> schedule, but the result is advisory: it reports drift an operator would
> otherwise find by accident. A human put them there, and absence is not
> evidence the human changed their mind.

## A learned model is inventory, not an exposure

A model that appears on a registered host is registered as a provider model —
and reaches nobody. Nothing creates a frontend model for it.

That is deliberate. Authorisation is granted on frontend models, so a host
starting an unrelated model must not hand existing principals access to it. New
inventory is closed by default; exposing it is a decision someone makes, by
pointing a frontend model at it.

## What survives a provider being deleted

- **Usage and spend.** `usage_events` records the model and provider name at
  ingest, so history outlives the row (migration 0031).
- **Frontend models and their targets.** A target is bound by name, so deleting
  the provider model leaves the target in place, unresolved — and re-registering
  the same model on the same provider reattaches routing with no manual step
  (migration 0036).
- **Grants**, which are held on frontend models rather than on provider models.

Each of those is there because the alternative was demonstrated: the provider
decomposition revoked live grants, would have failed the retention batch
outright, and silently halved a frontend model's capacity — all three found by
deploying and making a real request.
