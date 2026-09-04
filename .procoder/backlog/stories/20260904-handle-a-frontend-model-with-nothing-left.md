# Handle a frontend model that resolves to nothing

Status: open
Created: 2026-09-04
Epic: frontend-models-survive-their-targets
Sprint: sprint-10

## Description

As a caller, I get an answer that tells me what is wrong instead of one that sends me hunting for a typo.

The reported case: only the default target is defined, and it pointed at the dynamic model that was removed.

First match wins and a matching rule commits — if its targets resolve to nothing the request fails rather than falling through, because falling through would make the rule order depend on health the author cannot see. Static targets essentially never resolve to nothing; dynamic ones do by design, so a dynamic-only rule becomes a black hole on reboot. The answer is mixed targets in one rule, not a change to commit semantics.

## Acceptance criteria

- [ ] A frontend model with no routable target stays listed, stays permissioned, and is marked unavailable with the reason and last-seen time
- [ ] Requests use `/admin/fallback-model` when one is set
- [ ] Otherwise the response is `503 model_unavailable` naming what is missing, never `404`
- [ ] A rule mixing dynamic and cloud targets renormalises across the survivors when the dynamic one is leased out
- [ ] Creating a frontend model with a path that has no static or cloud floor emits a warning naming the rule, and is not rejected
- [ ] Verified on a Spark: stop the model, confirm the frontend model survives with its grants and returns 503 or the fallback

## Evidence
