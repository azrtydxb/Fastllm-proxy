# The registration and health service

Status: done 2026-09-04
Created: 2026-09-04
Milestone: self-registering-hosts
Issue: #13

## Description

A small service on each host that serves models. It registers the *provider* on
a lease and heartbeats; it dials the control plane and is never dialled, so it
works on a remote Docker host or a Kubernetes cluster the control plane cannot
reach into.

It does not push a model list. The control plane calls `GET /v1/models` itself,
because FastLLM has to reach the provider anyway in order to serve traffic —
letting the agent supply the list allows registering models the proxies cannot
dial, discovered later by a user at request time. Enumerating from the control
plane makes discovery and reachability the same test, and shrinks the service to
something worth trusting on a GPU host.

Every engine in play answers `GET /v1/models` — vLLM, SGLang, llama.cpp, TGI,
Ollama, Triton's OpenAI frontend, LM Studio, mlx-lm — so the agent never has to
identify the engine to do its job. Engine detection only enriches optional
metadata. An unknown engine degrades to "registered, no metadata", never to
"unsupported".

The same probe answers both questions that matter: whether the provider is
alive, and whether each registered name is still in its response. The second is
what catches a host that is healthy while serving something else, and it costs
one call per provider rather than one per model.

Cleanup is two-staged deliberately. Grace has to exceed a model load — the 27B
takes over ten minutes on a Spark and a reboot is routine — because suppressing
routing is reversible and deleting a row and its credential is not.
