# 0003 — The control plane enumerates a provider's models

Status: accepted
Date: 2026-09-04

## Context

A registration service on a model host has to get the list of served models to
FastLLM somehow. The obvious design has the agent call `GET /v1/models` locally
and push the result, since it is right there.

The registry drifted twice this week, and the instructive case was a provider
model pointing at a host that was healthy and answering while serving something
else entirely. `src/health.rs:70-75` probes and rotates, which answers "is this
address answering" and structurally cannot answer "is this address still what
the row claims".

## Decision

The agent registers the provider — an `api_base`, a node, a TTL — and nothing
else. The control plane calls `GET /v1/models` itself, both to learn the models
and, on a schedule, to check that each registered name is still in the response.

An agent-supplied list was rejected because FastLLM must reach the provider
anyway in order to serve traffic. A list pushed from the host can register models
the proxies cannot dial, and that failure surfaces at request time, to a user.
Enumerating from the control plane makes discovery and reachability the same
test, and it shrinks the service to something worth trusting on a GPU host:
register an address, heartbeat, exit.

The same call answers liveness and identity, at one probe per provider rather
than one per model.

## Consequences

Nothing can be registered that cannot be reached. Engine support costs nothing —
vLLM, SGLang, llama.cpp, TGI, Ollama, Triton, LM Studio and mlx-lm all answer
`GET /v1/models`, so the agent never identifies the engine to do its job, and an
unknown engine degrades to "no metadata" rather than "unsupported".

The price is that the control plane now makes outbound calls on a schedule to
every provider, which is new load and a new failure surface, and that a provider
reachable from its own host but not from the control plane cannot be registered
at all — correctly, but the error must say so clearly or it will read as a bug in
the agent.

Engine-specific metadata (context length, quantization, whether the engine emits
`reasoning` or `reasoning_content`) still needs an engine hint, so the agent
supplies one, as a hint and never as a requirement.
