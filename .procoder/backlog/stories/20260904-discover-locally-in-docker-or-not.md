# Discover locally, in Docker or not

Status: open
Created: 2026-09-04
Epic: the-registration-and-health-service
Sprint: sprint-9

## Description

As an operator, the service finds what is serving on this host regardless of how it was started.

Every engine in play answers `GET /v1/models` — vLLM, SGLang, llama.cpp, TGI, Ollama, Triton's OpenAI frontend, LM Studio, mlx-lm — so the agent never needs to identify the engine to do its job. Engine detection only enriches optional metadata such as context length, quantization, and whether the engine emits `reasoning` or `reasoning_content`.

## Acceptance criteria

- [ ] Static `api_base` list, port probe, Docker and Kubernetes are each optional and composable
- [ ] The port probe finds a bare process started by hand or by sparkrun, with no Docker socket present
- [ ] An unrecognised engine registers with no metadata rather than failing
- [ ] A Docker container's published port is used, never its bridge address
- [ ] Verified against vLLM and SGLang on a Spark, and against one containerised engine

## Evidence
