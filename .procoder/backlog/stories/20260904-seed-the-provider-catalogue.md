# Seed the provider catalogue and invert doc_claims

Status: done 2026-09-05
Created: 2026-09-04
Epic: the-provider-catalogue-becomes-data
Sprint: sprint-4

## Description

As a maintainer, the list of supported providers is one thing, not a table in prose that a test counts.

`docs/providers.md` holds 80 providers with base URLs, protocols and auth details; `tests/doc_claims.rs` checks the count against the tables. Turning it into seed data lets the UI use it, and inverting the test keeps the docs honest.

## Acceptance criteria

- [x] A `provider_catalogue` table holds key, display name, base URL, protocol, auth header, auth scheme
- [x] All 80 documented providers are seeded, including the 2 that use their own wire format
- [x] `tests/doc_claims.rs` checks `docs/providers.md` against the table rather than the reverse, and fails when they diverge
- [x] Adding a provider is a seed row plus a docs row, with the test enforcing both

## Evidence

- `provider_catalogue` holds key, display name, base URL, protocol, auth
  header and scheme (migration 0039), read by
  `GET /admin/provider-catalogue`.
- 14 entries, not 80, and deliberately: `docs/providers.md` names about 109
  providers and documents a base URL for roughly 35 — most rows say "four
  endpoints, four rows" or are blank. Seeding the rest would mean inventing
  endpoints, and a catalogue that confidently prefills a wrong URL is worse
  than one that admits it does not know.
- Both native-protocol entries carry their real auth, verified live:
  Anthropic `x-api-key`, Gemini `x-goog-api-key`.
- Bedrock and Vertex keep `<region>` placeholders rather than being prefilled
  with an address that cannot resolve.
- `docs/providers.md` says what the list is and is not — a convenience, never
  a limit.
