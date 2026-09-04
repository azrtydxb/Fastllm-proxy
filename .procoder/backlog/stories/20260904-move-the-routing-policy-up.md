# Move the routing policy to the frontend model

Status: open
Created: 2026-09-04
Epic: load-balancing-on-the-frontend-model
Sprint: sprint-3

## Description

As an operator, I set the balancing policy where the things being balanced actually are — and it does something when I set it.

Scoping this against the code found that it is not a schema move. `Policy`
selects between the backends *inside one pool* (`router.rs:156`), while a
frontend model's targets are chosen by weight and a deterministic prefix hash
on an entirely separate path that never consults `Policy`. After the
decomposition a provider model has one backend, so its pool has one element and
the policy already chooses between one thing. Moving the column alone would
produce a knob that silently does nothing.

`migrations/0028` added `policy` to `models` to choose between one model's replicas. After the decomposition a provider model has one provider and no replicas, so the column is stranded. It moves to the frontend model.

## Acceptance criteria

- [ ] Frontend-model target selection can use a policy, not only weights —
      `cache-affinity` across provider models, `least-loaded` and
      `lowest-latency` between them
- [ ] `policy` lives on the frontend model; the provider model no longer has one
- [ ] Existing values are carried across by the migration, including onto the frontend models generated for multi-backend models
- [ ] NULL still means "whatever the deployment was started with", as 0028 documents
- [ ] A proxy meeting a policy it does not know still falls back rather than refusing to route
- [ ] Two local replicas on `cache-affinity` and two cloud providers on `lowest-latency` coexist on kw

## Evidence
