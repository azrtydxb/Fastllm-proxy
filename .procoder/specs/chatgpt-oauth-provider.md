# Spec: ChatGPT Subscription / Codex OAuth Provider

## Problem

Users with a ChatGPT Pro subscription have access to OpenAI models through
the ChatGPT subscription tier, but FastLLM only supports API-key auth for
OpenAI. They cannot use their subscription as a provider in FastLLM without
an API key — which costs extra.

The desired architecture is for FastLLM to proxy subscription-backed requests
directly, without the Codex CLI or app-server in the hot path.

## Users

- **Operator** — has a ChatGPT Pro subscription, wants to add it as a provider
  in FastLLM, route requests through it, and monitor usage/limits.
- **Client** — sends requests to FastLLM's OpenAI-compatible API. Does not
  know the upstream is subscription-backed.

## In scope

- New `chatgpt_oauth` credential kind on `providers`
- OAuth PKCE flow: connect, callback, token exchange, storage
- Token refresh via `refresh_token`
- Snapshot decryption and header build
- Health/routing integration with existing failover chain

## Out of scope

- Native Codex app-server integration
- Per-request auth (tokens are cached and refreshed in background)
- Multi-tenant OAuth isolation (single operator per deployment)

## Constraints

- Never store or transmit the user's ChatGPT password
- Tokens must be encrypted at rest (FastLLM's existing AES-256-GCM mechanism)
- Rust hot path must not perform I/O for auth — tokens decrypted before
  request dispatch
- Streaming must remain end-to-end, not buffered

## Interfaces

**Database:** `providers.credential_kind` CHECK constraint gains
`chatgpt_oauth`. New column `providers.chatgpt_oauth_tokens BYTEA` encrypted
OAuth credential blob.

**Admin API:**

- `POST /admin/providers` accepts `credential_kind: "chatgpt_oauth"`
- `POST /admin/providers/:id/connect-chatgpt` starts OAuth flow (returns
  challenge URL + state)
- `GET /admin/providers/:id/oauth/callback` handles OAuth callback
- `POST /admin/providers/:id/chatgpt-disconnect` clears tokens
- `GET /admin/providers/:id/chatgpt-status` returns connection status

**Control plane → data plane (snapshot):**

- `BackendDef` carries a new `chatgpt_oauth_tokens` field (decrypted),
  populated in the build phase only when `credential_kind == "chatgpt_oauth"`

## Data

New DB columns:

```
providers.credential_kind:   CHECK (IN ('static', 'gcp_service_account', 'chatgpt_oauth'))
providers.chatgpt_oauth_tokens:  BYTEA  (AES-256-GCM encrypted JSON: {access_token, refresh_token, token_type, expires_in, created_at})
provider_catalogue.credential_kinds:  ('chatgpt_oauth,static') for openai entry
```

## Edge cases

- Token refresh fails mid-stream → 502, not a client error
- Token expiry race between refresh and request → use token until expiry,
  next request gets fresh token
- OAuth provider changes its API endpoint → 401 from health probe → mark
  unhealthy → fall back
- Subscription reaches limit → 429 from upstream → rate-limit signal →
  failover to next model/provider
- `refresh_token` also expires (long-lived) → user must reconnect

## Failure modes

| Failure                           | Proxy behaviour                                           | User-visible             |
| --------------------------------- | --------------------------------------------------------- | ------------------------ |
| Provider unreachable              | Health probe fails → mark unhealthy → failover            | 502 on fallback provider |
| Token expired, refresh fails      | Mark unhealthy → failover                                 | 502 on fallback provider |
| 401/403 from upstream             | Same as invalid key → mark unhealthy                      | 502 on fallback provider |
| 429 from upstream                 | Route as rate-limited → failover                          | 429 on fallback provider |
| OAuth callback lost (page closed) | Challenge still valid, user can re-navigate to /connect   | "Please connect ChatGPT" |
| DB corruption on token column     | Provider appears with blank tokens → 401 → mark unhealthy | 502 on fallback provider |

## Acceptance criteria

- [x] Provider catalogue has an OpenAI ChatGPT entry
- [x] Admin API accepts `credential_kind: chatgpt_oauth`
- [x] Connect ChatGPT button starts OAuth flow
- [x] PKCE challenge/verifier generated server-side
- [x] Callback exchange produces access and refresh tokens
- [x] Tokens stored encrypted in `providers.chatgpt_oauth_tokens`
- [x] Snapshot decrypts tokens before dispatch
- [x] Rust refreshes tokens if they are within 5 minutes of expiry
- [x] OAuth errors (401/403) mark backend as unhealthy
- [x] Subscription rate limits (429) trigger failover
- [x] Disconnect clears stored tokens
- [x] UI shows connection status

## Open questions

1. Which OpenAI OAuth endpoints does a ChatGPT Pro subscription use?
2. What scopes does the OAuth token include — is `chatgpt` separate from
   the platform `openid` scope?
3. How long do refresh tokens live for subscription OAuth vs API keys?
4. Can a subscription token be used on the regular `/v1/chat/completions`
   endpoint, or only on a subscription-specific endpoint?
5. What happens when the subscription's daily/hourly limit is hit — does
   it return `429` with a specific error, or something else?
6. Should the provider catalogue entry carry `chatgpt_oauth` in its
   `credential_kinds`?
