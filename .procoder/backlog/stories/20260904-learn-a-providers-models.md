# Learn a provider's models from the control plane

Status: done 2026-09-04
Created: 2026-09-04
Epic: the-registration-and-health-service
Sprint: sprint-8

## Description

As an operator, the models a provider serves appear on their own, named exactly as the provider exposes them.

The control plane calls `GET /v1/models` rather than trusting a list from the agent, because FastLLM must reach the provider anyway to serve traffic — so discovery and reachability become the same test, and a model the proxies cannot dial is never registered.

## Acceptance criteria

- [x] The control plane enumerates a dynamic provider's models itself; the register call carries no model list
- [x] Names are taken verbatim, with no mapping table and no normalisation
- [x] A provider the control plane cannot reach registers no models and says so
- [x] A model appearing or disappearing on a live provider is reflected without a restart
- [x] Verified on a Spark: start a model, see the provider model appear; swap it, see the set change

## Evidence

- The control plane enumerates; the register call carries no model list.
- Names are taken verbatim, qualified by provider only when the bare name is
  already taken.
- A provider the control plane cannot reach registers no models and records
  why — verified on kw: an unreachable address was degraded with
  `Connection refused (os error 111)`.
- Only dynamic providers learn. OpenRouter answers with hundreds of models and
  is a cloud provider, so no rows were created for it — confirmed by the
  provider count staying at 6 across sweeps.
