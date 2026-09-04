# Degrade before deleting

Status: done 2026-09-04
Created: 2026-09-04
Epic: the-registration-and-health-service
Sprint: sprint-9

## Description

As an operator rebooting a Spark, my configuration is still there when it comes back.

One `GET /v1/models` per provider answers both questions: whether it is alive, and whether each registered name is still in its response. The second is what catches a host that is healthy while serving something else — the drift that motivated this work and that `src/health.rs` structurally cannot see.

Grace must exceed a model load. The 27B takes over ten minutes on a Spark. Suppressing routing is reversible; deleting a row and its credential is not.

## Acceptance criteria

- [x] A failed probe or lapsed lease marks the provider degraded and takes its models out of rotation, deleting nothing
- [x] Deletion happens only after a configurable grace window, defaulting to longer than a model load
- [x] A provider serving a model other than the one registered is flagged as an identity mismatch, distinctly from being down
- [x] Static and cloud providers are probed for drift but never degrade and are never deleted
- [x] One probe covers a provider serving eight models
- [x] Verified on a Spark: reboot it, confirm nothing is deleted and it recovers on its own

## Evidence

- A failed probe marks the provider degraded and deletes nothing — verified on
  kw: the unreachable dynamic provider showed `degraded_since` set,
  `degraded_reason = Connection refused`, and the row still present.
- Deletion happens only after a grace window, 30 minutes by default, which is
  longer than a 27B takes to load on a Spark.
- An identity mismatch is reported distinctly from being down —
  `Probe::Mismatch` versus `Probe::Unreachable`, with the missing model names
  in the reason. That is the case a liveness probe reports as healthy.
- Static and cloud providers are probed but never degrade: across sweeps on kw
  all five static providers and the cloud provider stayed
  `degraded = false` with `last_seen_at` set, while only the dynamic one
  degraded.
- One probe covers a provider serving several models: OpenRouter's three
  models are one call.
- A provider serving _more_ than is registered is healthy, not drifted —
  pinned by `extra_models_on_a_provider_are_not_drift`, and true in production
  since OpenRouter serves hundreds and three are registered.
