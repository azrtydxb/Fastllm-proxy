# Authorise on the frontend model

Status: open
Created: 2026-09-04
Epic: rbac-on-frontend-models
Sprint: sprint-6

## Description

As an operator granting access, I grant it on the thing I expose.

`src/proxy.rs:267-278` authorises against the resolved concrete model so that "a virtual model routes access; it must never be able to grant it". That guards against someone who can edit routing rules but should not reach a given model — and that person cannot exist, because editing rules needs `config:write`, which is all-or-nothing (`validate_grant("config:write", "model/x")` is an error, only `*` validates) and already covers granting oneself any model.

Grant-by-name has held up only because a provider model's name never changes:
`PatchModel` has no `name` field, so the admin API cannot rename one. Migration
0029 was the first thing in this codebase to rename a model, and it revoked two
live roles' access on the first request after deploying. A registrar that
creates and deletes names on a lease does that continuously.

The existing test `a_virtual_models_grant_does_not_reach_its_targets_and_vice_versa` pins the old behaviour and has to be replaced deliberately, with an ADR, not quietly deleted.

## Acceptance criteria

- [x] `may_invoke` is checked against the frontend model the caller named
- [x] An ADR records the reversal and the `config:write` reasoning that justifies it
- [x] The old test is replaced by one pinning the new invariant, not removed
- [x] A principal granted one frontend model cannot reach another that shares a target
- [x] The `novagrade` principal on kw keeps working across the change, or is re-granted as part of it
- [x] Audit rows still record grants at the moment they are made
- [x] A provider model that is renamed, deleted or recreated does not change who
      can reach the frontend model in front of it — the case migration 0029
      broke by hand

## Evidence

- `may_invoke` is checked against the name the caller used: a request naming a
  frontend model is authorised against it, one naming a provider model
  directly against the provider model.
- ADR 0002 records the reversal and the `config:write` reasoning — the old rule
  guarded against someone who can edit routing rules but should not reach a
  target, and that person cannot exist because editing rules already grants
  everything.
- The old test was replaced rather than removed, in both directions: a grant on
  a frontend model now authorises it, _and_ must not unlock a provider model
  named directly. The failover suite's equivalent was renamed
  `a_frontend_model_grant_covers_the_chain_it_routes_to`.
- Migration 0035 carries existing grants across, granting a frontend model only
  to roles holding **every** target — proven both ways on clones of live data:
  `novagrade` gained `model/qwen3-6-35b-a3b`, and a role holding one of
  `embed`'s two targets gained nothing.
- `novagrade` keeps working across the change: **200** on its model and **403**
  on `gpt-5`, verified on kw after the deploy.
- Audit rows still record grants at the moment they are made; the audit layer
  is unchanged.
- `flatten_grants` had to stop dropping frontend model names — it kept only
  provider models, so a grant on a frontend model was discarded at
  snapshot-build time and `may_invoke` was always false.

Not closed by this story: removing direct provider-model addressing, which is
its sibling. Naming a provider model still works and is still authorised
against that model.
