# Working in this repository

## Documentation is part of the change, not a follow-up

**Before every commit, update the documentation that the change makes stale.**
Not "later", not a separate tidy-up commit — in the same commit as the code.
Check each of these and update the ones the change touches:

- `README.md` — user-facing behaviour, flags, config, endpoints, quickstarts.
- `docs/architecture.md` — the component and flow diagrams. If a change adds a
  component, a role, an endpoint that crosses a plane boundary, or alters how
  the snapshot moves, the diagram is now wrong and must be redrawn.
- `deploy/README.md` and `deploy/*.yaml` comments — anything an operator does.
- `TODO.md` — mark work done; delete claims that are no longer true.
- The doc comment on any function whose behaviour or contract you changed.

A comment that describes behaviour the code no longer has is worse than no
comment: it actively misleads the next reader. This has bitten this repo
repeatedly — a doc claiming credentials were encrypted before they were, a
manifest comment claiming the admin API was token-gated when it was gated on
nothing, a field comment promising a live-swapped snapshot that was set once
at startup. Every one of those was found by review, not by the author.

If a change makes a security claim in the docs false, fixing the claim is the
*minimum*; consider whether the code should be made to match the claim
instead.

## Environment

- **Never use Docker on this machine.** It is not the workflow. Do not run
  `docker`, do not try to start Docker Desktop, do not propose
  `docker compose` as the way to get a dependency running locally.
- **The `kw` Kubernetes cluster is the dev environment.** Build there, test
  there, deploy there. Dependencies a test needs get deployed to kw. CI already
  builds on in-cluster ARC runners — follow that pattern.
- Postgres for local test runs lives on kw and is reachable directly at a LAN
  VIP; the connection URL is kept in `/tmp/dburl`.
- Do not be precious about the dev cluster. Deploy and test on it freely.

## Conventions this codebase holds to

- **Doc comments explain *why*, not *what*.** The reasoning is the valuable
  part; the code already says what it does.
- **The request path performs no I/O.** No database call, no network call, no
  file read while serving a request. `tests/no_io_on_hot_path.rs` guards this
  and must be extended whenever new work lands on that path.
- **API keys hash with SHA-256; user passwords with Argon2id.** These are
  deliberately different: keys are high-entropy random, passwords are
  low-entropy and human-chosen. Do not unify them.
- **`AppState::apply_snapshot` is the single write path** for the snapshot. It
  rebuilds the routing registry in the same call so the two cannot diverge.
  Never write to the snapshot cell directly.
- Performance claims need a measurement. `docs/performance.md` records what was
  tried and rejected, with numbers, so nobody re-litigates it from intuition.

## Before you claim something works

Run it. `cargo test` passing is not the same as the feature working end to
end, and several real bugs on this branch — usage silently dropped for
non-streaming responses, readiness checks that treated 503 as "not started" —
passed the unit tests and failed the moment a real request went through.
