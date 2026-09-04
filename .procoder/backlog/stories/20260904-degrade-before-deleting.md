# Degrade before deleting

Status: open
Created: 2026-09-04
Epic: the-registration-and-health-service
Sprint: sprint-9

## Description

As an operator rebooting a Spark, my configuration is still there when it comes back.

One `GET /v1/models` per provider answers both questions: whether it is alive, and whether each registered name is still in its response. The second is what catches a host that is healthy while serving something else — the drift that motivated this work and that `src/health.rs` structurally cannot see.

Grace must exceed a model load. The 27B takes over ten minutes on a Spark. Suppressing routing is reversible; deleting a row and its credential is not.

## Acceptance criteria

- [ ] A failed probe or lapsed lease marks the provider degraded and takes its models out of rotation, deleting nothing
- [ ] Deletion happens only after a configurable grace window, defaulting to longer than a model load
- [ ] A provider serving a model other than the one registered is flagged as an identity mismatch, distinctly from being down
- [ ] Static and cloud providers are probed for drift but never degrade and are never deleted
- [ ] One probe covers a provider serving eight models
- [ ] Verified on a Spark: reboot it, confirm nothing is deleted and it recovers on its own

## Evidence
