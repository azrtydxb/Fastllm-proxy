# Add a cloud provider by picking it from the list

Status: done 2026-09-05
Created: 2026-09-04
Epic: the-provider-catalogue-becomes-data
Sprint: sprint-5

## Description

As an operator adding Groq, I choose it, paste one key, and I am done.

Today that means typing a base URL into a backend row and pasting the key once per model — which is also how the same key ends up encrypted N times.

## Acceptance criteria

- [x] Choosing a catalogue entry fills base URL, protocol, auth header and auth scheme
- [x] The key is entered once, stored on the provider, encrypted, and never returned
- [x] A custom base URL is still possible for a provider not in the catalogue
- [x] Anthropic and Gemini are addable this way, sending `x-api-key` and `x-goog-api-key` raw
- [x] Adding a real cloud provider on kw and serving one request through it

## Evidence

- Choosing a catalogue entry fills in base URL and protocol; the auth header
  and scheme come with it.
- The key is entered once, stored on the provider, encrypted, never returned.
- A custom base URL is still typeable for anything not in the list, which is
  most providers.
- Anthropic and Gemini are addable this way, sending their keys raw in
  `x-api-key` and `x-goog-api-key`.
- Verified live: the OpenRouter provider on kw serves `gpt-5` through one
  credential shared by three models.
