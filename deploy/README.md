# Deploying to the kw cluster

Plain manifests — this does not earn a Helm chart.

| | |
|---|---|
| Namespace | `fastllm` |
| Image | `192.168.10.123:5000/azrtydxb/fastllm-proxy/proxy:main` (zot, anonymous pull) |
| VIP | `192.168.10.126` via kube-vip — proxy traffic only, see below |
| Backends | spark1 `192.168.10.246:40013/v1` and spark2 `192.168.10.245:40045/v1`, both serving `qwen3-6-35b-a3b-nvfp4` |

## Split deployment

Two Deployments, one Postgres, since Task 12:

- **`fastllm-control`** (`control.yaml`) — `--role=control`. Database (a CloudNativePG `Cluster`, `fastllm-pg`), the admin API (`/admin/*` — keys, principals, roles, models, backends; see README.md for the full route table), `/snapshot` and `/usage`. A `ClusterIP` Service on port 4001, **not** on the LoadBalancer VIP.
- **`fastllm-proxy`** (`deployment.yaml`) — `--role=proxy`. Polls `fastllm-control`'s `/snapshot` over `FASTLLM_CONTROL_URL`, authenticating with `FASTLLM_PROXY_TOKEN`. Caches the last snapshot it received to an `emptyDir` at `/var/lib/fastllm`, so a pod restart while the control plane is down still comes up serving the last-known model/key set instead of crash-looping or refusing traffic.

### TLS on `/snapshot` and `/usage`

`/snapshot` carries `model_backends.upstream_api_key` in usable plaintext form — encrypted at rest in Postgres, but the proxy has to present it to the backend, so the *transport* has to be trusted wherever a backend has a real credential. This cluster's do, so `fastllm-control` terminates TLS on its admin listener (both `/admin/*` and `/snapshot`/`/usage` share the one listener — there is no way to TLS one route and not the others on the same port).

The cert comes from the in-cluster `cluster-ca` `ClusterIssuer` (the same one `novamail` and others use) via the `fastllm-control-tls` `Certificate` in `control.yaml`. cert-manager writes `tls.crt`/`tls.key` (mounted into `fastllm-control` at `/etc/fastllm/tls`, passed to `--tls-cert`/`--tls-key`) and `ca.crt` (mounted into `fastllm-proxy` at `/etc/fastllm/ca`, passed to `--ca-bundle`) into that one Secret — `cluster-ca` is a private CA no public root store trusts, so without `--ca-bundle` the proxy's TLS handshake to `fastllm-control` fails closed. `FASTLLM_CONTROL_URL` in `deployment.yaml` points at `https://fastllm-control.fastllm.svc:4001/snapshot` accordingly.

If `--tls-cert`/`--tls-key` are ever both removed (a dev cluster with no real backend credentials, say), `fastllm-control` falls back to plain HTTP rather than refusing to start — but it logs a startup warning every time, precisely because that fallback is silent otherwise and this is data that should not travel in the clear by accident.

### ⚠️ Keep the admin API and UI off the VIP

`fastllm-control`'s `/admin/*` requires a session cookie (`POST /login`, checked against `principals.password_hash` with Argon2id — see README.md's "Admin authentication" section and `src/control/auth.rs`). `/snapshot` and `/usage` are unchanged: they check `--proxy-token`, a separate shared secret for machine-to-machine polling and reporting (the proxy proving itself to the control plane), not a human login.

**A password is no longer the same thing as being an admin.** A valid session only proves *who* is calling; each route additionally requires a permission (`usage:read`, `key:create`, `key:revoke`, or `config:write` — see README.md's table) that the principal must hold through a granted role. Giving a service account a password so it can view the UI (`PUT /admin/principals/{id}/password`) no longer silently hands it full administrative reach — it can log in, but every route still 403s until a role granting the permission it needs is granted with `POST /admin/principals/{id}/roles`. The `admin` role (granted automatically to the very first login by `set-password`, see below) is still the one that can do everything.

**Network isolation is still the right default even with a real login in front of `/admin/*`.** A session cookie stops an anonymous request; it does not make brute-forcing a weak password, a leaked cookie, or a compromised pod on the same network segment a non-issue. Treat the login the same way you would treat any other internal admin tool's login: necessary, not sufficient, and no reason to put it on a public listener.

**That means `fastllm-control`'s Service must stay `ClusterIP`, and must never be merged into `fastllm-proxy`'s `LoadBalancer` Service on `192.168.10.126`.** That VIP is reachable from the whole LAN, and `fastllm-control` now also serves the management UI (`/`, `/ui/*`) on the same listener — another reason it belongs off the VIP, not fewer. `control.yaml` has this ClusterIP-only, with a comment at the top saying why; don't "simplify" it into one Service later without re-reading that comment.

### Bootstrapping the first admin login

A freshly migrated database has no session anyone can obtain — every `principals` row starts with `password_hash IS NULL`, so there is no way to `POST /login` successfully yet. Run once, from inside the cluster (same trust boundary as minting a key below):

```bash
kubectl -n fastllm exec deploy/fastllm-control -- \
  fastllm-proxy set-password --name admin --password "$(openssl rand -hex 16)" \
  --database-url "$FASTLLM_DATABASE_URL"
```

(`FASTLLM_DATABASE_URL` is already set in `fastllm-control`'s own environment — `kubectl exec` inherits it.) This creates the `admin` principal if it does not exist (as `kind = 'user'`), sets its password, and grants it the `admin` role unless it already holds one granting `config:write`. The condition is the *permission*, not "has any role": the seeded `bootstrap` principal already holds `inference` so keys minted against it can invoke models, and an earlier "has no role at all" check silently skipped the grant for it — producing an account that logged in and then got 403 everywhere, including from the routes needed to repair it. Save the password somewhere real (a password manager, not this terminal's scrollback) — `set-password` never prints it back. Safe to run again later to reset it.

## First install

Generate the proxy token — shared between `fastllm-control` and `fastllm-proxy`, checked only by `/snapshot`/`/usage`/`/limits/reconcile` (the machine-to-machine routes). It is not what protects `/admin/*` — that is the session-cookie login described below:

```bash
kubectl create namespace fastllm --dry-run=client -o yaml | kubectl apply -f -

kubectl -n fastllm create secret generic fastllm-proxy-token \
  --from-literal=token="$(openssl rand -hex 32)" \
  --dry-run=client -o yaml | kubectl apply -f -
# (control.yaml also ships a Secret with a placeholder token — the command
# above overwrites it before first deploy; keep doing this rather than
# committing a real token to git.)

# Also generate the encryption-at-rest key: fastllm-control encrypts
# model_backends.upstream_api_key with this before writing it to Postgres
# (see README.md's "Encryption at rest" section) and refuses to start
# without it. Same rotation caveat as the proxy token: don't hand-edit
# control.yaml's placeholder, overwrite it here.
kubectl -n fastllm create secret generic fastllm-encryption-key \
  --from-literal=key="$(openssl rand -hex 32)" \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl apply -f deploy/
# fastllm-control mounts fastllm-control-tls (the Certificate above) and
# fastllm-proxy mounts its ca.crt — both Deployments can sit in
# ContainerCreating for a few seconds on a from-scratch install until
# cert-manager finishes issuing it. `kubectl -n fastllm get certificate
# fastllm-control-tls` shows READY once it exists.
kubectl -n fastllm rollout status deploy/fastllm-control --timeout=240s
kubectl -n fastllm rollout status deploy/fastllm-proxy --timeout=240s
```

## Creating and using an API key

Keys are minted through the control plane's admin API, reachable only from inside the cluster (see the warning above) — `kubectl exec` into a pod that can reach the ClusterIP Service, or `kubectl -n fastllm exec` into `fastllm-control` itself. Log in first (see "Bootstrapping the first admin login" above for the very first one) to get a session cookie, then reuse it:

```bash
kubectl -n fastllm exec deploy/fastllm-control -- sh -c '
  curl -s --cacert /etc/fastllm/tls/ca.crt -c /tmp/cookie -XPOST https://localhost:4001/login \
    -H "content-type: application/json" -d "{\"name\":\"admin\",\"password\":\"$ADMIN_PASSWORD\"}" \
  && curl -s --cacert /etc/fastllm/tls/ca.crt -b /tmp/cookie -XPOST https://localhost:4001/admin/keys \
    -H "content-type: application/json" -d "{\"name\":\"my-client\",\"principal_id\":1}"
'
# {"id":7,"key":"sk-..."}
```

Or use the management UI at `https://fastllm-control.fastllm.svc:4001/` (port-forward it) instead of hand-writing `curl` — same admin API underneath, a form instead of JSON.

The response is the only time the plaintext key is ever shown — the database stores a SHA-256 hash, not the key, so read it now. Revoke the same way, cookie and all:

```bash
kubectl -n fastllm exec deploy/fastllm-control -- sh -c '
  curl -s --cacert /etc/fastllm/tls/ca.crt -c /tmp/cookie -XPOST https://localhost:4001/login \
    -H "content-type: application/json" -d "{\"name\":\"admin\",\"password\":\"$ADMIN_PASSWORD\"}" \
  && curl -s --cacert /etc/fastllm/tls/ca.crt -b /tmp/cookie -XDELETE https://localhost:4001/admin/keys/7
'
```

Revocation reaches the proxy within one poll interval (`--config-poll`, default 5s).

`principal_id: 1` above is the `bootstrap` service account the migrations seed,
which holds the `inference` role (every model). A key that should reach only
some models needs its own principal and a role that grants only those — all
through the same API, no SQL:

```bash
E() { kubectl -n fastllm exec deploy/fastllm-control -- "$@"; }

E curl -s --cacert /etc/fastllm/tls/ca.crt https://localhost:4001/admin/principals            # who exists, and their roles
E curl -s --cacert /etc/fastllm/tls/ca.crt https://localhost:4001/admin/roles                 # what each role grants
E curl -s --cacert /etc/fastllm/tls/ca.crt https://localhost:4001/admin/keys                  # prefix/name/principal/expiry only

E curl -s --cacert /etc/fastllm/tls/ca.crt -XPOST https://localhost:4001/admin/principals -H 'content-type: application/json' \
  -d '{"name":"eval-team"}'
# {"id":4,"name":"eval-team","kind":"service_account"}
E curl -s --cacert /etc/fastllm/tls/ca.crt -XPOST https://localhost:4001/admin/principals/4/roles -H 'content-type: application/json' \
  -d '{"role":"inference"}'
E curl -s --cacert /etc/fastllm/tls/ca.crt -XDELETE https://localhost:4001/admin/principals/4/roles/inference
E curl -s --cacert /etc/fastllm/tls/ca.crt -XDELETE https://localhost:4001/admin/principals/4   # also drops its keys and grants
```

`GET /admin/keys` never returns a key or its hash — the plaintext exists only in
the `POST` response above. A key granted narrower-than-`inference` access needs a
role holding just the models in question; `fastllm-proxy import` creates exactly
that shape (`import:<name>`) from a config file's `auth:` block, and
`GET /admin/roles` shows the result.

## Using it

```bash
curl http://192.168.10.126/v1/chat/completions \
  -H "Authorization: Bearer $KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"qwen3-6-35b-a3b-nvfp4",
       "messages":[{"role":"user","content":"hello"}],
       "max_tokens":400}'
```

`max_tokens` matters: Qwen3.6 is a thinking model and, with the qwen3 reasoning
parser, a short limit puts every token in `reasoning_content` and returns an
empty `content` — which looks like a broken deployment when it is not.

`/health` and `/metrics` need no auth, so probes and Prometheus work without a
key. Both expose backend addresses, so keep the VIP on the trusted network.

## Changing the model set

Models and backends now live in `fastllm-pg`, not the ConfigMap — `configmap.yaml`
only carries the `fastllm:` tuning block since the control/proxy split.
`fastllm-proxy import` is the supported way to seed or update them from a
LiteLLM-format file, and it is idempotent:

```bash
kubectl -n fastllm exec deploy/fastllm-control -- \
  fastllm-proxy import --config /path/to/litellm_config.yaml \
  --database-url "$FASTLLM_DATABASE_URL"
```

`import` encrypts `upstream_api_key` before writing it and requires
`FASTLLM_ENCRYPTION_KEY` to do so (see README.md's "Encryption at rest"
section) — running it via `kubectl exec` against `fastllm-control` picks that
up from the pod's own environment automatically, since `control.yaml` sets
it. Running `import` from anywhere else needs the same env var set by hand.

`import` runs in its own process, so `fastllm-control` picks its writes up on
the next periodic rebuild (`--snapshot-rebuild-interval`, default 5s) rather
than immediately. A change therefore lands on `fastllm-proxy` within that
interval plus one `--config-poll` interval (default 5s each, so worst case
~10s) — no rollout, no dropped generations.

### One-off changes, without a config file

For a single model or backend, the admin API is the supported route — it
publishes a rebuilt snapshot on the spot, so only `fastllm-proxy`'s poll
interval stands between the write and the change taking effect. Direct SQL
against `models`/`model_backends` is no longer the documented way to do any of
this; it bypasses the write path and waits on the periodic rebuild.

```bash
E() { kubectl -n fastllm exec deploy/fastllm-control -- "$@"; }

E curl -s --cacert /etc/fastllm/tls/ca.crt https://localhost:4001/admin/models     # models, backend ids, api_bases

E curl -s --cacert /etc/fastllm/tls/ca.crt -XPOST https://localhost:4001/admin/models -H 'content-type: application/json' \
  -d '{"name":"qwen3-6-35b-a3b-nvfp4"}'
# {"id":3,"name":"qwen3-6-35b-a3b-nvfp4"}

# Add a backend to that pool. upstream_model defaults to the model's own name;
# upstream_api_key is encrypted before it reaches Postgres and can never be
# read back — GET /admin/models reports only whether one is set.
E curl -s --cacert /etc/fastllm/tls/ca.crt -XPOST https://localhost:4001/admin/models/3/backends -H 'content-type: application/json' \
  -d '{"api_base":"http://192.168.10.245:40045/v1"}'
# {"id":9,...}

E curl -s --cacert /etc/fastllm/tls/ca.crt -XDELETE https://localhost:4001/admin/backends/9   # drop one replica
E curl -s --cacert /etc/fastllm/tls/ca.crt -XDELETE https://localhost:4001/admin/models/3     # drop the model and its backends
```

## Adding a provider

Most providers need no image change and no restart — a backend row is the
whole of it, and the proxy picks it up on the next snapshot rebuild. See
`README.md`'s provider table for verified base URLs. OpenRouter is the
shortest path to Anthropic and Gemini models *with* tool calling, because it
speaks OpenAI format:

```bash
kubectl -n fastllm port-forward svc/fastllm-control 4001:4001 &
curl -sk -c /tmp/ck -X POST https://127.0.0.1:4001/login \
  -H 'content-type: application/json' \
  -d '{"name":"bootstrap","password":"..."}'
curl -sk -b /tmp/ck -X POST https://127.0.0.1:4001/admin/models \
  -H 'content-type: application/json' -d '{"name":"claude-sonnet","description":"via OpenRouter"}'
curl -sk -b /tmp/ck -X POST https://127.0.0.1:4001/admin/models/$ID/backends \
  -H 'content-type: application/json' \
  -d '{"api_base":"https://openrouter.ai/api/v1",
       "upstream_model":"anthropic/claude-sonnet-4",
       "upstream_api_key":"sk-or-..."}'
```

Reaching Anthropic or Gemini directly means adding `"protocol":"anthropic"`
or `"protocol":"gemini"`. Two operational notes:

- Anthropic backends want `"default_max_tokens": 4096` (or whatever suits).
  Without it, any client request that omits `max_tokens` gets a 400 — the
  provider requires the field and the proxy will not invent a cap.
- Translated backends serve `/chat/completions` only, text messages only.
  Tool calling, images and the embeddings/audio endpoints return 501. Use an
  OpenRouter backend for those.

The control plane needs egress to the provider and its CA in the trust store;
public roots are already present in the image, so the hosted providers work
without `--ca-bundle`.

## When a Spark's port changes

GPUStack assigns the replica port and it moves on redeploy. If the backend goes
unhealthy, find the current one:

```bash
curl -H "Authorization: Bearer $GPUSTACK_KEY" \
  http://192.168.10.125/v2/model-instances
```

then repoint the backend. `api_base` is not editable in place — a backend is
identified by where it is, and the routing registry keys health and in-flight
state off that — so replace it: find the stale backend's id, delete it, and add
one at the new address.

```bash
E() { kubectl -n fastllm exec deploy/fastllm-control -- "$@"; }

E curl -s --cacert /etc/fastllm/tls/ca.crt https://localhost:4001/admin/models          # find the model id and the stale backend id
E curl -s --cacert /etc/fastllm/tls/ca.crt -XDELETE https://localhost:4001/admin/backends/9
E curl -s --cacert /etc/fastllm/tls/ca.crt -XPOST https://localhost:4001/admin/models/3/backends -H 'content-type: application/json' \
  -d '{"api_base":"http://192.168.10.245:40118/v1"}'
```

Both writes publish a rebuilt snapshot immediately, so this lands on
`fastllm-proxy` within one `--config-poll` interval (default 5s). Re-running
`import` against an updated config file also works and is the better choice
when several backends moved at once, at the cost of waiting for
`--snapshot-rebuild-interval` as well. This is the main thing a discovery
source would automate later.
