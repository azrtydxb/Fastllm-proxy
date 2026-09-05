# RBAC on frontend models

Status: done 2026-09-05
Created: 2026-09-04
Milestone: self-registering-hosts
Issue: #8

## Description

Two different things get called access control here. Credentials to a provider —
an OpenRouter key, a vLLM server's own auth — are not our RBAC; the provider or
engine defines them and we hold one and present it. Our RBAC is how we expose
models, and we expose them through frontend models.

The code disagrees. `src/proxy.rs:267-278` authorises against the resolved
concrete model, never the frontend name, on the stated grounds that "a virtual
model routes access; it must never be able to grant it".

That invariant defends against an actor who can edit routing rules but should
not reach some concrete model, and that actor cannot exist: editing rules needs
`config:write`, which is all-or-nothing (`validate_grant("config:write",
"model/x")` is an error, only `*` validates) and covers principals, roles,
models, passwords and everything else. Anyone who can redirect a frontend model
at `gpt-5` can simply grant themselves `gpt-5`.

Under dynamic registration it stops being merely unnecessary and becomes
harmful. Grants pinned to concrete rows evaporate on every model swap and do not
return when the model does, and a `model/*` wildcard silently extends to
whatever a provider starts exposing next, with nothing audited because nothing
was granted — a resource simply appeared that an existing grant matched.

This is the same decision as whether a client can still name a provider model
directly. If concrete addressing stays, frontend-only grants leave a bypass; if
it goes, frontend models are the only nameable surface and the only sensible
grantable subject. They move together.
