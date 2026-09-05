# Providers become records

Status: done 2026-09-05
Created: 2026-09-04

## Goal

A provider is something the system knows about, not something a screen infers.

Today `web/src/views/Providers.jsx:23` states it plainly — "a provider is a
grouping this screen invents, not a thing the API models" — and groups backends
by `api_base` origin at render time. The consequences are visible: the same
OpenRouter key is encrypted into one row per model, so rotating it is N edits;
and the 80 providers documented in `docs/providers.md` are prose you read rather
than a list you pick from.

Reaching this milestone means an operator can add Groq by choosing it from a
list, pasting one key, and importing the models they want — and that a provider
model is understood to belong to exactly one provider, which is what makes
everything in the next milestone possible.

Nothing here depends on the registration service. This milestone is worth
finishing on its own merits, and it is the keystone the dynamic work sits on.

## Done when

- `providers` is a table owning the endpoint, the credential, the protocol and
  the auth scheme; `model_backends` no longer exists.
- A provider model has exactly one provider, and names are unique per provider
  rather than globally.
- Load balancing is configured on the frontend model, over provider/model pairs,
  and is labelled as load balancing.
- The provider catalogue is seed data; `tests/doc_claims.rs` checks the docs
  against the table rather than the reverse.
- Every model that had two or more backends is serving through an auto-created
  frontend model, and no client that worked before this milestone is broken by
  it.
