# Learn a provider's models from the control plane

Status: open
Created: 2026-09-04
Epic: the-registration-and-health-service
Sprint: sprint-8

## Description

As an operator, the models a provider serves appear on their own, named exactly as the provider exposes them.

The control plane calls `GET /v1/models` rather than trusting a list from the agent, because FastLLM must reach the provider anyway to serve traffic — so discovery and reachability become the same test, and a model the proxies cannot dial is never registered.

## Acceptance criteria

- [ ] The control plane enumerates a dynamic provider's models itself; the register call carries no model list
- [ ] Names are taken verbatim, with no mapping table and no normalisation
- [ ] A provider the control plane cannot reach registers no models and says so
- [ ] A model appearing or disappearing on a live provider is reflected without a restart
- [ ] Verified on a Spark: start a model, see the provider model appear; swap it, see the set change

## Evidence
