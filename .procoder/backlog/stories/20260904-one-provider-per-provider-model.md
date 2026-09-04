# One provider per provider model, names unique per provider

Status: open
Created: 2026-09-04
Epic: provider-decomposition-and-the-provider-model-rename
Sprint: sprint-1

## Description

As an operator running the same model on two Sparks, I see two provider models rather than one model with two backends underneath it.

This is the invariant the whole design rests on. It makes balancing a frontend-model concern, and it forces name uniqueness down to the provider — two Sparks both expose `bge-m3`, and both are called `bge-m3`.

The consequence to settle here: with per-provider names a client can no longer address a provider model by a globally unique name, and `src/proxy.rs:267` resolves frontend names first and concrete ones second.

## Acceptance criteria

- [ ] A provider model references exactly one provider; the schema cannot express two
- [ ] Names are unique per provider, not globally
- [ ] Two providers exposing `bge-m3` coexist as separate provider models, verified on kw
- [ ] An ADR records whether concrete addressing is qualified or removed, and why
- [ ] A frontend model and a provider model can share a name without a 409, or the ADR says why it still cannot

## Evidence
