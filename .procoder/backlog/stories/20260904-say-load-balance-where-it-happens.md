# Say load balance where load balancing happens

Status: open
Created: 2026-09-04
Epic: load-balancing-on-the-frontend-model
Sprint: sprint-4

## Description

As an operator, I can find where to spread traffic across two providers without knowing that adding a second target to a rule is how you do it.

`VirtualModels.jsx:204` says "targets weighted and ordered" and renders weights as shares. That is a load balancer nobody named. The screen reads as routing rules.

## Acceptance criteria

- [ ] The frontend model screen names load balancing and shows the policy in effect
- [ ] Adding a second target presents as balancing, not as an extra row
- [ ] Weights are still shown as relative shares, and the docs still say they need not sum to 100
- [ ] `docs/` and the screenshots that show this screen are updated in the same commit

## Evidence
