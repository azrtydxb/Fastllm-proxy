# Add a cloud provider by picking it from the list

Status: open
Created: 2026-09-04
Epic: the-provider-catalogue-becomes-data
Sprint: sprint-5

## Description

As an operator adding Groq, I choose it, paste one key, and I am done.

Today that means typing a base URL into a backend row and pasting the key once per model — which is also how the same key ends up encrypted N times.

## Acceptance criteria

- [ ] Choosing a catalogue entry fills base URL, protocol, auth header and auth scheme
- [ ] The key is entered once, stored on the provider, encrypted, and never returned
- [ ] A custom base URL is still possible for a provider not in the catalogue
- [ ] Anthropic and Gemini are addable this way, sending `x-api-key` and `x-goog-api-key` raw
- [ ] Adding a real cloud provider on kw and serving one request through it

## Evidence
