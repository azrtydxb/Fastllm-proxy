# Move the routing policy to the frontend model

Status: done 2026-09-05
Created: 2026-09-04
Epic: load-balancing-on-the-frontend-model
Sprint: sprint-3

## Description

As an operator, I set the balancing policy where the things being balanced actually are — and it does something when I set it.

Scoping this against the code found that it is not a schema move. `Policy`
selects between the backends _inside one pool_ (`router.rs:156`), while a
frontend model's targets are chosen by weight and a deterministic prefix hash
on an entirely separate path that never consults `Policy`. After the
decomposition a provider model has one backend, so its pool has one element and
the policy already chooses between one thing. Moving the column alone would
produce a knob that silently does nothing.

`migrations/0028` added `policy` to `models` to choose between one model's replicas. After the decomposition a provider model has one provider and no replicas, so the column is stranded. It moves to the frontend model.

## Acceptance criteria

- [x] Frontend-model target selection can use a policy, not only weights —
      `cache-affinity` across provider models, `least-loaded` and
      `lowest-latency` between them
- [x] `policy` lives on the frontend model; the provider model no longer has one
- [x] Existing values are carried across by the migration, including onto the frontend models generated for multi-backend models
- [x] NULL still means "whatever the deployment was started with", as 0028 documents
- [x] A proxy meeting a policy it does not know still falls back rather than refusing to route
- [x] Two local replicas on `cache-affinity` and two cloud providers on `lowest-latency` coexist on kw

## Evidence

- `policy` lives on `frontend_models` and is gone from `provider_models`
  (migration 0038). Leaving it would have shown a knob governing nothing.
- Existing values carried across from the first target, which is safe because
  every target of one frontend model came from one pool when 0029 split it.
- NULL still means the weighted split, so every existing frontend model
  behaves exactly as it did.
- A proxy meeting a policy it does not know falls back rather than refusing to
  route: parsed in the snapshot builder, not constrained in the database.
- Verified on kw: `PATCH /admin/frontend-models/{id}` → 204, reads back
  `lowest-latency`, an unknown policy → 400, and the model still serves.
- `ModelDef::policy` stays in the snapshot and is unset from the database:
  `File` mode can still give one model several backends, where a pool really
  does have more than one member.
