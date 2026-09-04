# 0004 — Dynamic providers degrade before they are deleted

Status: accepted
Date: 2026-09-04

## Context

A host that dies cannot send "remove me", so absence has to be the signal that a
dynamically registered provider is gone. The simplest rule — a failed probe
deletes the provider and its models — is wrong in this environment.

A 27B load on a DGX Spark takes over ten minutes, during which the endpoint does
not answer. Reboots are routine; one happened this week. Under a delete-on-first-
failure rule, every restart would destroy the provider row and its encrypted
credential, and every model load would look like a decommissioning.

Deleting is also not symmetric with registering. Registration is idempotent and
cheap to repeat; deletion throws away a credential and, before the fix in the
usage epic, cascaded away billing history.

## Decision

Two stages. A failed probe or a lapsed lease marks the provider degraded: its
models go out of rotation and nothing is deleted. Only after a configurable grace
window — defaulting to longer than a model load — are the provider and its
learned models removed.

Static and cloud providers never degrade and are never deleted. They get the same
probe, but only as advisory drift detection, because a human put them there and
absence is not evidence that the human changed their mind.

A single-stage rule was rejected for the reasons above. A pure soft-delete with
no removal at all was rejected because the registry then accumulates every model
ever run on every host, which is the drift this work exists to end, in slower
form.

## Consequences

Rebooting a host is safe and self-healing: it comes back, the lease refreshes,
routing resumes, and nothing was lost. An identity mismatch — the provider is up
but serving something else — is reported distinctly from being down, which is the
case that motivated the work and the one no liveness probe can see.

The price is a window in which the registry knowingly describes something that is
not currently servable. Frontend models must therefore cope with targets that
exist but do not resolve, which is what mixed static and dynamic targets in one
rule are for.

The grace window is a tuning knob that will be wrong somewhere: too short and a
slow model load looks like a decommission, too long and a genuinely retired host
lingers. It is configurable per provider for that reason, and its default is
chosen from a measured model load rather than from intuition.
