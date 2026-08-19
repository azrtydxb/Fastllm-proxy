# The quality gate

The repository is governed by procoder, a formatting-and-hygiene gate that
runs on every write and on every commit. Its policy files live under
`.procoder/` in the repository root; edit those to change what the gate
enforces. This page is the command roster, so a contributor can discover
what the tooling offers without leaving the docs.

## Everyday commands

- `procoder doctor` — which formatters this repository needs, which are
  installed, and how to install the rest.
- `procoder init` — install the missing formatters, every command visible
  before it runs.
- `procoder check` — the commit gate over your changed files; unchecked
  counts as failing.
- `procoder format` — print a file's formatted result so you can review it
  before writing it.
- `procoder audit` — every domain's checks over the whole tree; how this
  repository was first brought in line.

## Per-domain reports

- `procoder lint` — the canonical linter per ecosystem over your changes.
- `procoder security` — secrets over the changed files (blocking); `--deep`
  adds SAST and dependency vulnerabilities over the whole repository.
- `procoder ci` — workflow hygiene: pinned actions, job timeouts,
  concurrency cancellation.
- `procoder infra` — Dockerfiles, Terraform, Kubernetes manifests, Helm
  charts, where those files exist.
- `procoder docs` — broken references, diagram drift, required docs, README
  structure; `--external` adds link checking.
- `procoder git` — the pre-finish status: branch, hygiene, message checks,
  templates.
- `procoder maintain` — dead-code candidates, complexity, function length;
  judgment calls, never blocking.

## The index and the ledger

- `procoder index` — the code index built from ctags and SCIP: find,
  search, refs, outline, impact.
- `procoder debt` — harvest `debt:` markers into a ledger; a marker with no
  revisit trigger is flagged as rot.

## Workflow commands

- `procoder spec` — the gap-closing interview that produces a complete
  specification.
- `procoder plan` — turn an approved spec into an implementation plan.
- `procoder todo` — the quality-gated task list; a task only closes when
  the controller agrees it is done.

## Plumbing

- `procoder agents` — keep per-host agent rule files in sync with
  `AGENTS.md`.
- `procoder templates` — print the default `.procoder/` policy files.
- `procoder principles` — the engineering principles text the gate is built
  around.
- `procoder lessons` — lessons recorded from past sessions.
- `procoder scrub` — scrub transcripts before sharing them.
- `procoder hook` — the write-hook entry point the harness calls; not for
  humans.
- `procoder version` — which version answered.
