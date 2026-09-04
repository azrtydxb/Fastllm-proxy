# Authorise on the frontend model

Status: open
Created: 2026-09-04
Epic: rbac-on-frontend-models
Sprint: sprint-6

## Description

As an operator granting access, I grant it on the thing I expose.

`src/proxy.rs:267-278` authorises against the resolved concrete model so that "a virtual model routes access; it must never be able to grant it". That guards against someone who can edit routing rules but should not reach a given model — and that person cannot exist, because editing rules needs `config:write`, which is all-or-nothing (`validate_grant("config:write", "model/x")` is an error, only `*` validates) and already covers granting oneself any model.

The existing test `a_virtual_models_grant_does_not_reach_its_targets_and_vice_versa` pins the old behaviour and has to be replaced deliberately, with an ADR, not quietly deleted.

## Acceptance criteria

- [ ] `may_invoke` is checked against the frontend model the caller named
- [ ] An ADR records the reversal and the `config:write` reasoning that justifies it
- [ ] The old test is replaced by one pinning the new invariant, not removed
- [ ] A principal granted one frontend model cannot reach another that shares a target
- [ ] The `novagrade` principal on kw keeps working across the change, or is re-granted as part of it
- [ ] Audit rows still record grants at the moment they are made

## Evidence
