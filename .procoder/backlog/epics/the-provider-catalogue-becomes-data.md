# The provider catalogue becomes data

Status: done 2026-09-05
Created: 2026-09-04
Milestone: providers-become-records
Issue: #12

## Description

`docs/providers.md` documents 80 working providers with their base URLs, and
`tests/doc_claims.rs` checks the count against the tables. None of it is usable:
adding a cloud provider means typing a base URL into a backend row and pasting
the key once per model.

The catalogue already knows what the form needs — base URL, protocol (78 reached
as-is, 2 through their own wire format), and the auth header and scheme each one
wants, since Gemini authenticates with `x-goog-api-key` and Anthropic with
`x-api-key`, both raw rather than behind a scheme (`migrations/0013`). It is all
prose.

Turning it into seed data makes adding Groq a matter of choosing it, pasting one
key, and importing the models you want. `tests/doc_claims.rs` inverts: the docs
are checked against the table, so the table cannot drift away from what is
documented.

Cloud imports are offered, never automatic. OpenRouter's `/v1/models` returns
around 400 and nobody wants 400 rows — which is the deliberate difference from
dynamic providers, where the entire point is that no human is in the loop.
