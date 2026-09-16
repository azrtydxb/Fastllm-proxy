# Subscription rate limits (429) trigger failover

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

When a subscription is rate-limited (429), the request fails over to the next model/provider.

## Acceptance criteria

- [x] Subscription rate limits (429) trigger failover

## Evidence

`src/proxy.rs` line 1005: "429 joins 5xx as retryable, and it is the reason cross-model failover exists: a hosted provider answering 'rate limited'". 429 responses from upstream backends are treated as retryable, triggering the outer loop to try the next model or deployment fallback. This works for OAuth backends the same as any other backend type.
