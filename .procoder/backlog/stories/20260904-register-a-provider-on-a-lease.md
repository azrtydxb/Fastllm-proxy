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

- [x] `POST /admin/providers/register` creates or refreshes a dynamic provider under a lease
- [x] The call is idempotent and safe to repeat every heartbeat
- [x] The service authenticates with a principal API key scoped to its own node, not an admin session
- [x] A compromised node can rewrite only its own providers
- [x] One host registering two providers on different ports is supported, verified on a Spark
- [x] The advertised address is configured, never inferred from discovery

## Evidence

- `POST /admin/providers/register` creates or refreshes a dynamic provider
  under a lease — verified on kw: a new address returned
  `{kind: dynamic, leased: true}` and the row carries node, engine and
  `lease_expires_at`.
- Idempotent by address, so a heartbeat is the same operation each time.
- The agent authenticates with a principal API key, not an admin session.
  `/admin/*` accepted a session cookie only, which would have 401'd every
  heartbeat; a bearer key now authenticates and the route takes
  `provider:register` rather than `config:write`.
- A compromised node can rewrite only its own providers, and this is enforced
  rather than merely recorded: a key held on `node/spark-246` registering
  `spark-245` returned **403** naming both. `principal_has_permission` matches
  by verb alone, so this needed a resource-aware check — a scope stored and
  never checked reads on a permission matrix as though it were enforced.
- One host, two ports: the five LAN endpoints on the two Sparks are five
  providers, and the agent discovers exactly those five.
- The advertised address is configured, never inferred.
- Registering an address that is already a _static_ provider returns
  `kind=static, leased=false` — registration cannot convert a human's provider
  into one that expires.
