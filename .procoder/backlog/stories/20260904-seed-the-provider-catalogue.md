# Seed the provider catalogue and invert doc_claims

Status: open
Created: 2026-09-04
Epic: the-provider-catalogue-becomes-data
Sprint: sprint-4

## Description

As a maintainer, the list of supported providers is one thing, not a table in prose that a test counts.

`docs/providers.md` holds 80 providers with base URLs, protocols and auth details; `tests/doc_claims.rs` checks the count against the tables. Turning it into seed data lets the UI use it, and inverting the test keeps the docs honest.

## Acceptance criteria

- [ ] A `provider_catalogue` table holds key, display name, base URL, protocol, auth header, auth scheme
- [ ] All 80 documented providers are seeded, including the 2 that use their own wire format
- [ ] `tests/doc_claims.rs` checks `docs/providers.md` against the table rather than the reverse, and fails when they diverge
- [ ] Adding a provider is a seed row plus a docs row, with the test enforcing both

## Evidence
