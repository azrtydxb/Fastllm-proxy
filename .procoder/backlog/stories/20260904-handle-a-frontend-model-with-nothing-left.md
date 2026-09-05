# Handle a frontend model that resolves to nothing

Status: done 2026-09-05
Created: 2026-09-04
Epic: frontend-models-survive-their-targets
Sprint: sprint-10

## Description

As a caller, I get an answer that tells me what is wrong instead of one that sends me hunting for a typo.

The reported case: only the default target is defined, and it pointed at the dynamic model that was removed.

First match wins and a matching rule commits — if its targets resolve to nothing the request fails rather than falling through, because falling through would make the rule order depend on health the author cannot see. Static targets essentially never resolve to nothing; dynamic ones do by design, so a dynamic-only rule becomes a black hole on reboot. The answer is mixed targets in one rule, not a change to commit semantics.

## Acceptance criteria

- [x] A frontend model with no routable target stays listed, stays permissioned, and is marked unavailable with the reason and last-seen time
- [x] Requests use `/admin/fallback-model` when one is set
- [x] Otherwise the response is `503 model_unavailable` naming what is missing, never `404`
- [x] A rule mixing dynamic and cloud targets renormalises across the survivors when the dynamic one is leased out
- [x] Creating a frontend model with a path that has no static or cloud floor emits a warning naming the rule, and is not rejected
- [x] Verified on a Spark: stop the model, confirm the frontend model survives with its grants and returns 503 or the fallback

## Evidence

- A frontend model with no routable target stays listed and permissioned, and
  answers **503 `model_unavailable`** naming the target it wanted — verified
  on kw.
- The deployment fallback is reached first where one is set. It does not make
  a provider model addressable, which would reopen the direct addressing that
  frontend-only naming closed.
- 503 and not 404: the frontend model exists, is named correctly and is
  permissioned; its providers are gone. That is a condition of the deployment,
  it resolves itself when a host returns, and it is retriable.
- Where the 503 lives took two attempts. The empty-candidate branch does not
  catch it, because targets bind by name so a frontend model _keeps_ them when
  its providers go — the list is never empty in the case that matters, and the
  request fell through to a 404 one layer down. Found by making the call
  against the deployed cluster; pinned by an end-to-end test.
- That 404 also listed every model in the deployment, to a caller who had just
  been refused. Provider models are not client-facing names now, so it
  advertised a surface the caller cannot use. Removed.
