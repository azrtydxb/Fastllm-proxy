# 0005 — Per-provider names wait for frontend-only addressing

Status: accepted
Date: 2026-09-04
Supersedes: 0001

## Context

ADR 0001 concluded that model names become unique per provider, on the
reasoning that two Sparks both serving `bge-m3` should both call it `bge-m3`
and that frontend models are left as the only global namespace. That part of
0001 was wrong about what the code could carry, and implementing it surfaced
why.

Routing identifies a model by its **name**, in two places. `build_virtual_models`
resolves a frontend model's targets to `models.name` and puts that string in the
snapshot, and `resolve_target_models` takes a candidate name and looks the model
back up by it. Two rows sharing a name therefore collapse into one target: the
migration would split `bge-m3` across two providers, build a frontend model with
two targets, and send every request to whichever row the lookup found first —
silently, with both providers healthy and the UI showing the split correctly.

Grants have the same shape. A permission is `model:invoke` on `model/<name>`, so
two models sharing a name cannot be granted separately.

Neither is incidental. Both are the surfaces ADR 0002 moves to the frontend
model, and until that lands there is no identifier for a provider model other
than its name.

## Decision

Model names stay globally unique through the provider decomposition. Migration
0029 adds `UNIQUE (provider_id, name)` alongside the existing global constraint
rather than instead of it, so the intended shape is recorded without letting the
database into a state routing gets wrong.

Models split by the migration are qualified with their provider —
`bge-m3@192.168.10.245:8890` — and the clean name goes to the generated frontend
model, which is what clients actually call. The original row is renamed too:
leaving it unqualified would work, since a frontend model shadows a concrete one
of the same name during resolution, but it would leave a concrete model
permanently unreachable by its own name, which reads as a bug to whoever meets
it next.

Per-provider names land with ADR 0002's change, which replaces name-based
routing and name-based grants with the frontend model as the only addressable
and grantable surface. That is the change that can carry them.

The alternative — qualifying every model's snapshot identity now
(`provider/name`) — was rejected for this step. It reaches routing, usage
attribution and every existing `model/<name>` grant at once, so it would turn a
contained schema change into a change of the authorisation surface, which is
ADR 0002's decision to make rather than a side effect of this one.

## Consequences

The provider decomposition ships without waiting on the authorisation work, and
every invariant routing already depends on stays true throughout. A database
migrated by 0029 behaves identically to one that was not, for every caller.

The price is cosmetic and temporary: a model that had two backends comes out
with a machine-made name. It is inventory rather than a client-facing name —
the frontend model holds that — but anyone reading the models list during this
window will see it and wonder.

It also means `UNIQUE (provider_id, name)` is, for now, weaker than the
constraint next to it and enforces nothing on its own. That is deliberate: it
documents the target shape and starts costing nothing the moment the global
constraint is dropped.
