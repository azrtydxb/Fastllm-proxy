# Contributing

## The one rule that shapes everything else

**The request path performs no I/O.** No database call, no network call, no file
read while serving a request. Authorisation reads a pre-flattened in-memory
snapshot; rate limits and budgets are in-process counters. The proxy's measured
overhead against a real vLLM is zero because of this, and every feature added
since has been a temptation to slip one more lookup onto that path.

`tests/no_io_on_hot_path.rs` is what makes that enforceable rather than
aspirational. If your change adds work to the hot path, extend that test.

## Before you open a pull request

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test                                    # no database needed
cargo test --features control -- --include-ignored   # needs DATABASE_URL
cd web && npm test                            # if you touched the UI
```

CI runs all of these plus a classifier-feature pass, so a green local run is a
good predictor. `DATABASE_URL` points at any Postgres; the database tests are
`#[ignore]`d by default so `cargo test` stays runnable with nothing installed.

## Documentation is part of the change

Not a follow-up, not a separate tidy-up commit — the same commit as the code.
Check each of these and update the ones your change touches:

- `README.md` — user-facing behaviour, flags, config, endpoints, quickstarts.
- `docs/architecture.md` — the component and flow diagrams. A new component, a
  new role, an endpoint that crosses a plane boundary, or a change to how the
  snapshot moves means the diagram is now wrong.
- `docs/api.md`, `docs/operations.md` — routes and anything an operator does.
- `deploy/README.md` and the manifests' comments.
- `TODO.md` — mark work done; delete claims that stopped being true.
- The doc comment on any function whose contract you changed.

A comment describing behaviour the code no longer has is worse than no comment:
it actively misleads the next reader. This has bitten the repo repeatedly — a
doc claiming credentials were encrypted before they were, a manifest comment
claiming the admin API was token-gated when it was gated on nothing. Every one
was found by review, not by the author.

## Comments explain _why_

The code already says what it does. The reasoning is the part that cannot be
recovered from a later reading, so that is what a comment is for — especially
the alternative you rejected and the measurement that made you reject it.

## Claims need evidence

- **Performance claims need a measurement.** `docs/performance.md` records what
  was tried and rejected, with numbers, so nobody re-litigates it from
  intuition. Two of those entries exist because someone proposed the
  optimisation a second time.
- **Countable claims in the docs need a test.** `tests/doc_claims.rs` checks the
  numbers in the README against the tables they count. The README once said "20
  providers" in one bullet and "Twenty-three" in the next paragraph, with the
  table disagreeing with both.
- **"It works" needs a run.** `cargo test` passing is not the same as the
  feature working end to end. Several real bugs on this project passed the unit
  tests and failed the moment a real request went through — usage silently
  dropped for non-streaming responses, readiness checks that treated 503 as
  "not started".

## Never edit an applied migration

`sqlx` checksums every migration file and refuses to start when an applied one
changes — including a comment. Correcting a stale comment in an applied
migration failed every database test with "migration 14 was previously applied
but has been modified", and it failed in CI rather than locally, because the
checksum lives in the database rather than the repo.

If a migration's comment is wrong, fix it where the code lives, or add a new
migration.

## Adding a provider

Usually nothing to write. Anything speaking the OpenAI API is a row in a config
file or a `POST /admin/models/{id}/backends`. A provider that wants its key in
its own header (Azure OpenAI's `api-key`) is `auth_header` plus an empty
`auth_scheme`. Only a genuinely different wire format — a fourth alongside
OpenAI, Anthropic and Gemini — is code, and that lands in `src/protocol/`.

If you add one to the README's table, `tests/doc_claims.rs` will tell you the
count above it now disagrees.

## Adding an endpoint

A `POST` that carries `model` in its body is one line in `PROXIED_SUFFIXES`.
Native backends refuse anything but `/chat/completions` with a 501, so a new
passthrough endpoint cannot silently be handed to a translator that has no idea
what it is.

Stateful job APIs (`/batches`, `/files`, `/fine_tuning`) are not this. Retrieval
is a `GET` with no model and no body, so there is nothing to route on without
remembering which backend owns which id — durable state on the request path.
That needs a design.

## Commit messages

Say what changed and why it was wrong before. The bodies in `git log` are long
on purpose: they carry the reasoning and the measurement, which is the part that
does not survive in the diff.
