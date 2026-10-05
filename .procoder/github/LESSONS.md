# Lessons — findings that escaped our own gates

One entry per finding caught downstream (bot review, human review,
production) — the escape is the bug; the finding is its symptom. Every
entry names which layer should have caught it and the adaptation that now
does. `procoder lessons` flags entries with no adaptation.

Entry shape (unindented in real entries):

    ## <date> <where caught> — <one-line finding>

    - Class: mechanical | judgment | taste
    - Missed by: linter | rubric | controller | test | ci
    - Adaptation: <the concrete change that catches this class from now on>

== then register the commit template (once per clone):
git config commit.template .procoder/github/COMMIT_TEMPLATE.md

## 2026-10-05 PR 41 Copilot review — an unlanded edit, a stale cache, a doc that contradicted itself

- Class: mechanical
- Missed by: rubric
- Adaptation: a change the PR description promises (here `release-agent`
  needing the new `agent` job) is checked in the final diff before the PR
  is opened — a scripted edit can miss its anchor silently. REVIEW.md gains
  "every claim in the PR description is visible in the diff".

- Class: judgment
- Missed by: test
- Adaptation: a cache derived from a remote answer is overwritten on every
  valid answer, including an empty one, and a test drives the
  value-then-empty sequence. REVIEW.md gains the line.

- Class: judgment
- Missed by: rubric
- Adaptation: when a doc paragraph changes a stated contract, grep the same
  document (and docs/) for other statements of that contract and update
  them in the same PR. REVIEW.md gains the line.
