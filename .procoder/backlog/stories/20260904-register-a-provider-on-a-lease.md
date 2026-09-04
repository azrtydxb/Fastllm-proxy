# Register a provider on a lease

Status: open
Created: 2026-09-04
Epic: the-registration-and-health-service
Sprint: sprint-8

## Description

As an operator, a host tells FastLLM what it is serving and keeps saying so, and I never edit the registry to keep up.

The service registers the provider — an `api_base`, a node name, an engine hint and a TTL — and heartbeats. It dials the control plane and is never dialled, so a remote Docker host or a Kubernetes cluster the control plane cannot reach into still works.

A provider is an `api_base`, not a host: vLLM on `:8000` and SGLang on `:8001` are two providers.

## Acceptance criteria

- [ ] `POST /admin/providers/register` creates or refreshes a dynamic provider under a lease
- [ ] The call is idempotent and safe to repeat every heartbeat
- [ ] The service authenticates with a principal API key scoped to its own node, not an admin session
- [ ] A compromised node can rewrite only its own providers
- [ ] One host registering two providers on different ports is supported, verified on a Spark
- [ ] The advertised address is configured, never inferred from discovery

## Evidence
