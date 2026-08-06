# Deploying to the kw cluster

Plain manifests — this does not earn a Helm chart.

| | |
|---|---|
| Namespace | `fastllm` |
| Image | `192.168.10.123:5000/azrtydxb/fastllm-proxy/proxy:main` (zot, anonymous pull) |
| VIP | `192.168.10.126` via kube-vip — proxy traffic only, see below |
| Backend | spark2 `192.168.10.245:40045/v1`, model `qwen3-6-35b-a3b-nvfp4` |

## Split deployment

Two Deployments, one Postgres, since Task 12:

- **`fastllm-control`** (`control.yaml`) — `--role=control`. Database (a CloudNativePG `Cluster`, `fastllm-pg`), the admin API (`POST /admin/keys`, `DELETE /admin/keys/{id}`) and `/snapshot`. A `ClusterIP` Service on port 4001, **not** on the LoadBalancer VIP.
- **`fastllm-proxy`** (`deployment.yaml`) — `--role=proxy`. Polls `fastllm-control`'s `/snapshot` over `FASTLLM_CONTROL_URL`, authenticating with `FASTLLM_PROXY_TOKEN`. Caches the last snapshot it received to an `emptyDir` at `/var/lib/fastllm`, so a pod restart while the control plane is down still comes up serving the last-known model/key set instead of crash-looping or refusing traffic.

### ⚠️ The admin API has no authentication — keep it off the VIP

`fastllm-control`'s `/admin/*` (`POST /admin/keys`, `DELETE /admin/keys/{id}`) has **no authentication at all** — not `--proxy-token`, not anything else. It checks no header and no credential; anyone who can reach port 4001 can mint or revoke an API key. Only `/snapshot` checks `--proxy-token`, and that token is a shared secret for machine-to-machine polling (the proxy proving itself to the control plane), not admin authentication — there is no session, no password, no user identity behind either route. Real admin auth (principals with Argon2id passwords, sessions) is specified but deferred to the management-UI phase (see the repo root `TODO.md` and `docs/superpowers/specs/2026-08-06-control-plane-rbac-routing-design.md`).

**Network isolation — the `ClusterIP` Service below — is therefore the only control `/admin/*` has.** Do not read the token requirement on `/snapshot` as implying `/admin/*` is protected too; it is not, and this document previously said otherwise.

**That means `fastllm-control`'s Service must stay `ClusterIP`, and must never be merged into `fastllm-proxy`'s `LoadBalancer` Service on `192.168.10.126`.** That VIP is reachable from the whole LAN. Putting `/admin/keys` on it turns "reachable from any machine on the network" into "can mint an API key for this proxy from any machine on the network" — unauthenticated key creation, not a probe-and-metrics exposure like `/health`. `control.yaml` has this ClusterIP-only, with a comment at the top saying why; don't "simplify" it into one Service later without re-reading that comment.

## First install

Generate the proxy token — shared between `fastllm-control` and `fastllm-proxy`, checked only by `/snapshot` (the data plane's read of policy). It protects nothing on `/admin/*`, which has no authentication at all:

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
kubectl -n fastllm rollout status deploy/fastllm-control --timeout=240s
kubectl -n fastllm rollout status deploy/fastllm-proxy --timeout=240s
```

## Creating and using an API key

Keys are minted through the control plane's admin API, reachable only from inside the cluster (see the warning above) — `kubectl exec` into a pod that can reach the ClusterIP Service, or `kubectl -n fastllm exec` into `fastllm-control` itself:

```bash
kubectl -n fastllm exec deploy/fastllm-control -- \
  curl -s -XPOST localhost:4001/admin/keys -H 'content-type: application/json' \
  -d '{"name":"my-client","principal_id":1}'
# {"id":7,"key":"sk-..."}
```

The response is the only time the plaintext key is ever shown — the database stores a SHA-256 hash, not the key, so read it now. Revoke the same way:

```bash
kubectl -n fastllm exec deploy/fastllm-control -- \
  curl -s -XDELETE localhost:4001/admin/keys/7
```

Revocation reaches the proxy within one poll interval (`--config-poll`, default 5s).

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

(There is no admin-API route for model/backend CRUD yet — `import` and direct
SQL against `models`/`model_backends` are the only two ways today.) Neither
goes through the admin API, so `fastllm-control` only picks either up on its
own periodic rebuild (`--snapshot-rebuild-interval`, default 5s) rather than
immediately the way an admin API write does. A change lands on `fastllm-proxy`
within that interval plus one `--config-poll` interval (default 5s each, so
worst case ~10s) — no rollout, no dropped generations.

## When spark2's port changes

GPUStack assigns the replica port and it moves on redeploy. If the backend goes
unhealthy, find the current one:

```bash
curl -H "Authorization: Bearer $GPUSTACK_KEY" \
  http://192.168.10.125/v2/model-instances
```

then update `model_backends.api_base` for that row (re-run `import` against an
updated config file, or `UPDATE` it directly) and wait for `fastllm-control`'s
next periodic rebuild plus `fastllm-proxy`'s next poll (see above). This is
the main thing a discovery source would automate later.
