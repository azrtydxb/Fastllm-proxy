# Authentication, sessions and TLS

How the admin plane authenticates humans, where the credentials live, and
which listener has to be TLS.

## Admin authentication

Every `/admin/*` route (including `PUT /admin/principals/{id}/password` below) requires a valid session cookie, checked by `require_session` in `src/control/api.rs`. `POST /login` verifies a `{"name":..., "password":...}` body against `principals.password_hash` (Argon2id — see `src/control/auth.rs`'s doc comment for why this is a different hash from `api_keys.hash`'s SHA-256, deliberately: a password is low-entropy and human-chosen, an API key or session token is high-entropy random) and, on success, sets `fastllm_session` (`HttpOnly`, `SameSite=Strict`, `Secure` when TLS is on) valid for 12 hours. `POST /logout` deletes the session and clears the cookie.

`--proxy-token` still gates `/snapshot`, `/usage` and `/limits/reconcile` — those are proxy *processes* authenticating to the control plane, not humans, and have no password to present; sessions and the proxy token are deliberately separate mechanisms for separate callers.

**A session alone is not enough.** `require_session` only establishes *who* is calling; every `/admin/*` handler additionally checks *what* that principal may do, via `RequirePermission` (`src/control/api.rs`), against the same `roles → role_permissions → permissions` model `migrations/0001_init.sql` seeds and the data-plane's own `model:invoke` authorisation already uses. A session with no matching permission gets 403, not 200 — a principal that can log in is not, by that fact alone, an administrator. This closes what was previously a real gap: any principal a password was ever set for (via `PUT /admin/principals/{id}/password`) was a full admin, because nothing checked further than "is this a valid session".

Every admin route needs one of four permissions, seeded by `migrations/0001_init.sql`:

| Permission | Routes |
|---|---|
| `usage:read` | Every `GET /admin/*` route (keys, principals, models, virtual models, roles, limits, budgets, health) |
| `key:create` | `POST /admin/keys` |
| `key:revoke` | `DELETE /admin/keys/{id}` |
| `config:write` | Every other write: principals (create/delete/roles/**password**), models, backends, virtual models, routing rules and targets, limits, budgets |

There is no finer-grained permission for "manage principals" or "manage virtual models" than `config:write` — the schema does not seed one, and inventing a permission per table would multiply roles for no operator-visible benefit. The built-in `operator` role holds everything except `model:invoke` (i.e. all four of the above); `admin` holds everything including `model:invoke`. A role with `usage:read` alone can list and view but never create, revoke or reconfigure anything — the shape a read-only UI viewer or an audit tool needs.

**Bootstrapping the first login.** A freshly migrated database has no session anyone can obtain — every `principals` row starts with `password_hash IS NULL`. Run this once, with the same database access `import` already requires:

```bash
fastllm-proxy set-password --name admin --password '...' --database-url postgres://...
```

Creates the named principal if it does not exist yet (as `kind = 'user'`), sets its password, and grants it the `admin` role unless it already holds one granting `config:write` — the one way to reach every `config:write`/`key:create`/`key:revoke`/`usage:read` route before any session-driven role grant is possible. Safe to run again later to reset a forgotten password. `PUT /admin/principals/{id}/password` (session-gated *and* `config:write`-gated, for every login *after* the first) does the same password-setting step through the admin API/UI once at least one session exists — deliberately behind `config:write`, not merely a session: it is the route that hands a principal a working login, so only a caller already trusted to reconfigure the system may grant one to somebody else.

**Keep the admin port off the *gateway's* listener.** A session cookie stops an anonymous request; it does not make a brute-forced password or a leaked cookie a non-issue, and the admin port also serves `/snapshot`, which returns decrypted upstream credentials to anything holding the proxy token. So the admin port must never share an address with the data plane, whose callers hold inference keys and have no business reaching it.

Whether it gets its own reachable address is a deployment decision about *that* network. `deploy/control.yaml` gives it one — a separate `LoadBalancer` on a pinned VIP, TLS-only, distinct from the gateway's — on the reasoning that the honest alternative was not "unreachable" but "every operator port-forwards first". On a network you do not control, bind it to a cluster-internal Service or localhost instead. Either way, it stays a separate Service from the proxy's.

## Management UI

`--role all`/`control` serve a React dashboard from `/` and `/ui/*` (`src/control/ui.rs`; frontend source in `web/`). `--role proxy` serves no UI at all; `control::api::serve`, where the UI's fallback route is mounted, is never called for that role.

Thirteen screens, all driven by the admin API above: **Overview** (fleet, backends, traffic, recent changes), **Metrics**, **Usage & spend**, **Providers**, **Models**, **Virtual models** with the routing dry-run, **Prompt classes** with the leave-one-out evaluation, **API keys**, **Principals & roles** with the permission matrix and per-model grants, **Limits & budgets**, **Audit log**, **Fleet**, and **Settings**.

**Nothing on a screen is invented.** Where the control plane cannot answer a question, the UI says so and names what can: per-backend latency percentiles are per process and do not merge, so the Metrics screen prints the `histogram_quantile` query rather than an average of p99s; a model with no price shows `unpriced`, never `$0.00`; a backend no replica has probed shows a grey dot, not a green one. The rule is the same one the docs follow — a number nobody can reproduce is worse than an absent one.

The one thing that *is* computed in the browser is a rate: the control plane stores no metric history, so every line on the Metrics screen is a delta between two polls of the counters the fleet reports, starting empty when the page loads. The header says so on the page.

Three checks guard it, all under `web/` (`npm test`, plus a CI job and the Dockerfile's web stage):

- **`test/render.mjs`** mounts every screen against stubbed responses and fails on a render error or missing content. `npm run build` proves the modules parse; it says nothing about whether a screen renders, and a component used but not imported is a clean build and a blank page.
- **`test/interact.mjs`** clicks every control on every screen (231 of them) and then asserts the exact method, path and body that the important mutations send. This exists because the worst bug this UI has had was a screen that rendered perfectly while posting `{position, match_condition: {...}}` to a handler that flattens the conditions — serde discarded them, answered 201, and every rule created through the UI matched every request. Nothing looked wrong; only the request body was, and no test had ever looked at one.
- **`test/browser.mjs`** (`npm run test:browser`, needs a running control plane) drives the built bundle in headless Chrome: a real login, every screen, the dry-run against the live routing engine, and a second pass at 1280px. jsdom does no layout at all — every element is zero by zero and nothing can overlap — so the entire visual half of the UI was unverified by the two harnesses above. This one checks what only a browser knows: console errors, failed requests, sideways page scroll, zero-size or clipped controls, text overflowing its box. It writes a screenshot per screen to `web/.screenshots/` to be looked at, and launches Chrome with its own `--user-data-dir` so it never touches a browser already open.
- **`test/verify-fixtures.mjs`** (`npm run test:fixtures`, needs a reachable control plane) compares the fixtures against a live API. Both harnesses are only as truthful as their fixtures, and twice a fixture written from the Rust *field name* rather than the wire format hid a real bug while the suite stayed green — the flatten above, and `model:invoke` with resource `*` where the API stores `model/*`.

Embedded into the binary with [`rust-embed`](https://docs.rs/rust-embed) reading `web/dist/` at compile time — one container image, no second artefact to deploy. Built by the `Dockerfile`'s dedicated `node` stage, not a `build.rs` that shells out to `npm`, so `cargo build`/`cargo test` never require Node — a `web/dist/` empty at compile time (the normal state outside the Docker build) degrades to a plain "UI not available" response rather than failing the build. See `web/dist/.gitkeep`'s neighbour, `src/control/ui.rs`'s module doc comment, for the full mechanics.

## Encryption at rest

`model_backends.upstream_api_key` is encrypted at rest with AES-256-GCM (`src/control/secrets.rs`; `ring::aead`, already in the dependency tree via rustls) before `import`/the admin API ever write it to Postgres, and decrypted by `build_snapshot` when the control plane builds a snapshot. This protects the **database**, not the **snapshot**: `/snapshot` still carries the credential in usable plaintext form, because the proxy has to present it to the backend as a bearer token — an upstream credential cannot be reduced to a hash the way `api_keys.hash` is. `/snapshot` must be TLS wherever a backend has a real credential, exactly as before this existed. What encryption at rest actually buys: someone with read access to Postgres (a backup, a replica, a leaked `pg_dump`) no longer gets every upstream credential for free.

`--role control`/`all` and `fastllm-proxy import`/`reencrypt-backends` all require `FASTLLM_ENCRYPTION_KEY` — 32 bytes, hex-encoded (e.g. `openssl rand -hex 32`) — and refuse to start without it rather than falling back to plaintext. `--role proxy` never touches the database and never requires it. A database that already has plaintext rows from before this existed needs the one-shot `fastllm-proxy reencrypt-backends --database-url <url>` command run once; see `migrations/0004_encrypted_upstream_api_key.sql` for why this is a command rather than a format the read path silently tolerates forever.

## TLS on `/snapshot` and `/usage`

`/snapshot` carries `model_backends.upstream_api_key` in usable plaintext form (see "Encryption at rest" above), so it — and `/usage`, gated by the same token and sharing the same listener — must be TLS in any deployment where a backend has a real credential.

`--role control`/`all` take `--tls-cert`/`--tls-key` (PEM, `FASTLLM_TLS_CERT`/`FASTLLM_TLS_KEY`). Give both and the admin API — `/admin/*`, `/snapshot`, `/usage`, all of it, since they share one listener — serves HTTPS via `rustls`/`tokio-rustls` (already dependencies; no new TLS crate). Give neither and it serves plain HTTP, logging a startup warning every time it does, because a dev deployment with no real backend credentials is legitimate and must not be forced to generate a cert it does not need — but the fallback must never be silent. Giving only one of the two is a startup error, not a silent fall-back to HTTP.

On the client side, `--role proxy` in `Http` mode (`--control-url https://...`) and any `https://` backend `api_base` both go through the one pooled `Upstream` client (`src/upstream.rs`), which already speaks TLS. `--ca-bundle` (`FASTLLM_CA_BUNDLE`) adds one or more PEM CA certificates to the trust store *in addition to* the system roots — required to trust a private or self-signed cert (a cert-manager-issued, in-cluster control-plane certificate is the normal case; see `deploy/control.yaml`'s `fastllm-control-tls` `Certificate` and `deploy/README.md`'s TLS section) that no public root store contains. Without it, `--control-url https://...` against such a cert fails the handshake.
