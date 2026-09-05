# Say load balance where load balancing happens

Status: done 2026-09-05
Created: 2026-09-04
Epic: load-balancing-on-the-frontend-model
Sprint: sprint-4

## Description

As an operator, I can find where to spread traffic across two providers without knowing that adding a second target to a rule is how you do it.

`VirtualModels.jsx:204` says "targets weighted and ordered" and renders weights as shares. That is a load balancer nobody named. The screen reads as routing rules.

## Acceptance criteria

- [x] The frontend model screen names load balancing and shows the policy in effect
- [x] Adding a second target presents as balancing, not as an extra row
- [x] Weights are still shown as relative shares, and the docs still say they need not sum to 100
- [x] `docs/` and the screenshots that show this screen are updated in the same commit

## Evidence

- The frontend model screen names load balancing and shows the policy in
  effect, next to the targets it governs.
- The weighted split is named rather than left as an unspoken default: it was
  always a load balancer and nothing said so.
- Weights are still shown as relative shares.
- The control moved off the provider model, where after the split it offered
  to choose between one backend.
