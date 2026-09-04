# Import models from a cloud provider by choosing them

Status: open
Created: 2026-09-04
Epic: the-provider-catalogue-becomes-data
Sprint: sprint-5

## Description

As an operator, I browse what a cloud provider offers and import the few I want.

OpenRouter's `/v1/models` returns around 400. Auto-registering them would be unusable, which is the deliberate difference from dynamic providers, where no human is in the loop by design.

## Acceptance criteria

- [ ] A cloud provider's models can be listed from its `/v1/models`
- [ ] Nothing is registered until the operator selects it
- [ ] A provider that does not implement `/v1/models` still allows a model to be added by hand
- [ ] Importing from the live OpenRouter provider on kw registers only what was selected

## Evidence
