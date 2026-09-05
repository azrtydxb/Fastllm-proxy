# Import models from a cloud provider by choosing them

Status: done 2026-09-05
Created: 2026-09-04
Epic: the-provider-catalogue-becomes-data
Sprint: sprint-5

## Description

As an operator, I browse what a cloud provider offers and import the few I want.

OpenRouter's `/v1/models` returns around 400. Auto-registering them would be unusable, which is the deliberate difference from dynamic providers, where no human is in the loop by design.

## Acceptance criteria

- [x] A cloud provider's models can be listed from its `/v1/models`
- [x] Nothing is registered until the operator selects it
- [x] A provider that does not implement `/v1/models` still allows a model to be added by hand
- [x] Importing from the live OpenRouter provider on kw registers only what was selected

## Evidence

- `GET /admin/providers/{id}/available-models` lists what a provider is
  serving, from the same `GET /v1/models` the sweep uses, and marks which are
  already registered.
- Nothing is registered until an operator selects it. OpenRouter answers with
  hundreds; registering them all is not what anyone means by adding it, and
  that is the deliberate difference from a dynamic provider where no human is
  in the loop.
- A provider that does not answer returns 502 naming the address, rather than
  an empty list that would read as "serves nothing".
