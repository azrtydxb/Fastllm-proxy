# Provider catalogue has an OpenAI ChatGPT entry

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

A provider in the catalogue can be chosen with `credential_kind: "chatgpt_oauth"` as one of its valid credential types.

## Acceptance criteria

- [x] Provider catalogue has an OpenAI ChatGPT entry

## Evidence

- Migration 0051 updates `provider_catalogue` openai entry: `credential_kinds = 'static,chatgpt_oauth'` — confirmed on kw via `kubectl exec`