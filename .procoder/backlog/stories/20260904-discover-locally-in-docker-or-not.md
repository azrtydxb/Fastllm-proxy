# Discover locally, in Docker or not

Status: done 2026-09-04
Created: 2026-09-04
Epic: the-registration-and-health-service
Sprint: sprint-9

## Description

As an operator, the service finds what is serving on this host regardless of how it was started.

Every engine in play answers `GET /v1/models` — vLLM, SGLang, llama.cpp, TGI, Ollama, Triton's OpenAI frontend, LM Studio, mlx-lm — so the agent never needs to identify the engine to do its job. Engine detection only enriches optional metadata such as context length, quantization, and whether the engine emits `reasoning` or `reasoning_content`.

## Acceptance criteria

- [x] Static `api_base` list, port probe, Docker and Kubernetes are each optional and composable
- [x] The port probe finds a bare process started by hand or by sparkrun, with no Docker socket present
- [x] An unrecognised engine registers with no metadata rather than failing
- [x] A Docker container's published port is used, never its bridge address
- [x] Verified against vLLM and SGLang on a Spark, and against one containerised engine

## Evidence

- Static list, port probe, Docker and Kubernetes are each optional and
  composable; the port probe alone needs no container runtime.
- Verified against the live Sparks: the agent independently discovered exactly
  the five LAN providers already registered — `.245:8000`, `.245:8890`,
  `.246:8000`, `.246:8890`, `.246:8891`.
- That run is also what found the first version's doubled `/v1/v1/models`,
  which is why the check is a real endpoint rather than a mock.
- An unrecognised engine registers with no metadata rather than failing:
  nothing in the agent or the control plane branches on engine.
- vLLM on the Sparks and OpenRouter in the cloud were both probed by the same
  code path on kw, which is the uniform-engine claim demonstrated in
  production rather than asserted.
