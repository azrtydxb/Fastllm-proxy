# Deploying to the kw cluster

Plain manifests, applied continuously to one cluster. The reusable form is the [Helm chart](../charts/fastllm-proxy).

Not on Kubernetes? [`docker-compose.split.yml`](docker-compose.split.yml) in this directory is the same
two-plane split on a single host, and [docs/operations.md](../docs/operations.md#choosing-a-shape)
walks through all five shapes from a bare binary up to this one.

|           |                                                                                                             |
| --------- | ----------------------------------------------------------------------------------------------------------- |
| Namespace | `fastllm`                                                                                                   |
| Image     | `192.168.10.123:5000/azrtydxb/fastllm-proxy/proxy:main` (zot, anonymous pull)                               |
| VIP       | `192.168.10.126` via kube-vip — proxy traffic only, see below                                               |
| Backends  | spark1 `192.168.10.246:40013/v1` and spark2 `192.168.10.245:40045/v1`, both serving `qwen3-6-35b-a3b-nvfp4` |

> **These are one cluster's manifests, not a template.** They carry concrete
> values for the cluster they run on — a private registry at `192.168.10.123`,
> kube-vip VIPs at `.125`/`.126` (gateway) and `.129` (admin), and a
> `cluster-ca` ClusterIssuer. They are kept concrete on purpose: a manifest
> full of `<PLACEHOLDER>` cannot be applied, so it silently stops being tested,
> and these are applied continuously.
>
> **For your own cluster, use the Helm chart in [`charts/fastllm-proxy`](../charts/fastllm-proxy)**,
> which takes all of the above as values. If you would rather adapt these
> directly, the cluster-specific values are:
>
> | where                             | what to change                                                                |
> | --------------------------------- | ----------------------------------------------------------------------------- |
> | `control.yaml`, `deployment.yaml` | `image:` — registry host and pinned digest                                    |
> | `service.yaml`, `control.yaml`    | `kube-vip.io/loadbalancerIPs` — or drop the annotation and let your LB assign |
> | `control.yaml`                    | the `Certificate`'s `issuerRef` and its `ipAddresses` SAN                     |
> | `control.yaml`                    | the CNPG `Cluster` — or point `FASTLLM_DATABASE_URL` at your own Postgres     |

## The gateway's two addresses

`192.168.10.125` and `192.168.10.126` both answer, both select the same pods,
and both are pinned in `deploy/service.yaml` — `fastllm-proxy` holds the first,
`fastllm-proxy-alt` the second.

Two, rather than one, because this VIP has moved between those addresses more
than once and each move broke every client still holding the old one. A request
to a departed VIP fails to connect, which a caller experiences as a hang rather
than as an error naming the cause; it took a live debugging session to find that
"the API is hanging" meant "the address moved".

The moves were never kube-vip drifting. `deploy/service.yaml` pinned `.126`
while a second, differing manifest pinned `.125`, so whichever was applied last
won. **Pin addresses in this file and nowhere else** — that is the part that
actually stops it recurring.

Two Service objects rather than one with a comma-separated
`kube-vip.io/loadbalancerIPs`: the kube-vip running on this cluster brings up
only the first address of such a list. It was tried, and it silently assigned
one address while the annotation claimed two.

## Split deployment

Two Deployments, one Postgres, since Task 12:

- **`fastllm-control`** (`control.yaml`) — `--role=control`. Database (a CloudNativePG `Cluster`, `fastllm-pg`), the admin API (`/admin/*` — keys, principals, roles, models, backends; see README.md for the full route table), `/snapshot` and `/usage`. A `LoadBalancer` Service on port 4001, pinned to `192.168.10.129` — its own VIP, deliberately not the gateway's. See "The admin API and UI are on their own VIP" below.
- **`fastllm-proxy`** (`deployment.yaml`) — `--role=proxy`. Polls `fastllm-control`'s `/snapshot` over `FASTLLM_CONTROL_URL`, authenticating with `FASTLLM_PROXY_TOKEN`. Caches the last snapshot it received to an `emptyDir` at `/var/lib/fastllm`, so a pod restart while the control plane is down still comes up serving the last-known model/key set instead of crash-looping or refusing traffic.

### TLS on `/snapshot` and `/usage`

`/snapshot` carries `providers.upstream_api_key` in usable plaintext form — encrypted at rest in Postgres, but the proxy has to present it to the backend, so the _transport_ has to be trusted wherever a backend has a real credential. This cluster's do, so `fastllm-control` terminates TLS on its admin listener (both `/admin/*` and `/snapshot`/`/usage` share the one listener — there is no way to TLS one route and not the others on the same port).

The cert comes from the in-cluster `cluster-ca` `ClusterIssuer` (the same one `novamail` and others use) via the `fastllm-control-tls` `Certificate` in `control.yaml`. cert-manager writes `tls.crt`/`tls.key` (mounted into `fastllm-control` at `/etc/fastllm/tls`, passed to `--tls-cert`/`--tls-key`) and `ca.crt` (mounted into `fastllm-proxy` at `/etc/fastllm/ca`, passed to `--ca-bundle`) into that one Secret — `cluster-ca` is a private CA no public root store trusts, so without `--ca-bundle` the proxy's TLS handshake to `fastllm-control` fails closed. `FASTLLM_CONTROL_URL` in `deployment.yaml` points at `https://fastllm-control.fastllm.svc:4001/snapshot` accordingly.

If `--tls-cert`/`--tls-key` are ever both removed (a dev cluster with no real backend credentials, say), `fastllm-control` falls back to plain HTTP rather than refusing to start — but it logs a startup warning every time, precisely because that fallback is silent otherwise and this is data that should not travel in the clear by accident.

### ⚠️ The admin API and UI are on their own VIP

`fastllm-control`'s `/admin/*` requires a session cookie (`POST /login`, checked against `principals.password_hash` with Argon2id — see README.md's "Admin authentication" section and `src/control/auth.rs`). `/snapshot` and `/usage` are unchanged: they check `--proxy-token`, a separate shared secret for machine-to-machine polling and reporting (the proxy proving itself to the control plane), not a human login.

**A password is no longer the same thing as being an admin.** A valid session only proves _who_ is calling; each route additionally requires a permission (`usage:read`, `key:create`, `key:revoke`, or `config:write` — see README.md's table) that the principal must hold through a granted role. Giving a service account a password so it can view the UI (`PUT /admin/principals/{id}/password`) no longer silently hands it full administrative reach — it can log in, but every route still 403s until a role granting the permission it needs is granted with `POST /admin/principals/{id}/roles`. The `admin` role (granted automatically to the very first login by `set-password`, see below) is still the one that can do everything.

**A session cookie is necessary, not sufficient.** It stops an anonymous request; it does not make brute-forcing a weak password, a leaked cookie, or a compromised pod on the same segment a non-issue. Treat the login the way you would any internal admin tool's login.

**`fastllm-control` is a `LoadBalancer` on its own pinned VIP, `192.168.10.129`.** It was `ClusterIP` until 2026-08-12, and this section used to say it must stay that way. What changed is not the risk assessment but the honest alternative: the choice was never "no exposure", it was "every operator runs `kubectl port-forward` first", and the effect of that was an admin plane nobody opened on a LAN whose gateway VIPs (`.125`/`.126`, authenticated by the same key material) were always reachable.

The exposure rests on three things, and if any stops being true this goes back to `ClusterIP`:

1. the listener is TLS-only, so neither the session cookie nor the proxy token crosses the LAN in the clear;
2. `/admin/*` and the UI require a session cookie, Argon2id-checked;
3. `/snapshot` requires `--proxy-token` — which is now the credential whose leak costs the most, since it returns **decrypted upstream credentials**. Rotate `fastllm-proxy-token` if you suspect it, and restart both Deployments.

**It still must never be merged into `fastllm-proxy`'s `LoadBalancer` Service.** Those are the _gateway's_ addresses, held by callers with data-plane API keys and no business reaching the admin plane; one Service carrying both would put the admin port behind whatever gets opened up for the gateway next. Separate Services, separate addresses.

The certificate carries `192.168.10.129` as an IP SAN, so clients given the cluster CA verify it fully by address:

```bash
kubectl -n fastllm get secret fastllm-control-tls -o jsonpath='{.data.ca\.crt}' | base64 -d > /tmp/kwca.crt
curl --cacert /tmp/kwca.crt https://192.168.10.129:4001/healthz     # no -k needed
```

A browser still warns (`ERR_CERT_AUTHORITY_INVALID`): `cluster-ca` is a private CA in no OS trust store, and the IP SAN removes the name mismatch, not the unknown authority. Trust `cluster-ca` on your own machine if you want a warning-free UI.

### Bootstrapping the first admin login

A freshly migrated database has no session anyone can obtain — every `principals` row starts with `password_hash IS NULL`, so there is no way to `POST /login` successfully yet. Run once, from inside the cluster (same trust boundary as minting a key below):

```bash
kubectl -n fastllm exec deploy/fastllm-control -- \
  fastllm-proxy set-password --name admin --password "$(openssl rand -hex 16)" \
  --database-url "$FASTLLM_DATABASE_URL"
```

(`FASTLLM_DATABASE_URL` is already set in `fastllm-control`'s own environment — `kubectl exec` inherits it.) This creates the `admin` principal if it does not exist (as `kind = 'user'`), sets its password, and grants it the `admin` role unless it already holds one granting `config:write`. The condition is the _permission_, not "has any role": the seeded `bootstrap` principal already holds `inference` so keys minted against it can invoke models, and an earlier "has no role at all" check silently skipped the grant for it — producing an account that logged in and then got 403 everywhere, including from the routes needed to repair it. Save the password somewhere real (a password manager, not this terminal's scrollback) — `set-password` never prints it back. Safe to run again later to reset it.

## Managed by the operator

This cluster's two Deployments are reconciled by
[the operator](../operator), from [`fastllmproxy.yaml`](fastllmproxy.yaml) —
so `kubectl -n fastllm get fllm` is the state of the deployment, the
management UI has a **Deployment** screen that edits it, a rotated Secret
rolls the pods that read it, and an image change rolls the control plane
before the gateway rather than both at once.

The manifests in this directory (`control.yaml`, `deployment.yaml`,
`service.yaml`, `configmap.yaml`) are what the deployment was built from and
remain the readable description of it — but they are no longer what is
applied. Editing one and running `kubectl apply` will be undone on the next
reconcile. Edit the `FastllmProxy` instead, or the UI.

The operator itself runs in `fastllm-system`, two replicas behind a Lease so a
node drain does not leave the cluster without a controller, with its own
`/metrics`, `/healthz` and `/readyz`:

```bash
kubectl apply -f operator/deploy/crd.yaml -f operator/deploy/rbac.yaml -f operator/deploy/operator.yaml
kubectl -n fastllm-system set image deploy/fastllm-operator \
  operator=192.168.10.123/azrtydxb/fastllm-proxy/operator:sha-<commit>
```

(The manifest ships the public `ghcr.io` tag; this cluster pulls the sha build
from its own registry, same as the proxy.)

### Adopting it (what was actually done)

A running deployment cannot simply be handed over: `spec.selector` is
immutable on a Deployment and the operator's selector
(`app.kubernetes.io/*`) is not the one the manifests used (`app: fastllm-*`),
so a server-side apply is rejected outright. Both gateway VIPs also select
`app: fastllm-proxy`, so pods without that label would leave `.125` and
`.126` with no endpoints.

The cutover that keeps the gateway serving throughout:

```bash
# 1. Back up what cannot be regenerated: the database, and the manifests as
#    they stand.
kubectl -n fastllm exec fastllm-pg-1 -- pg_dump -U postgres -d fastllm --no-owner > fastllm.sql
kubectl -n fastllm get deploy,svc,cm,pdb -o yaml > fastllm-manifests.yaml

# 2. Note the ReplicaSets that are about to be orphaned, by name. Orphaning a
#    Deployment leaves its ReplicaSet behind still managing its pods, so
#    deleting the pods later would only make it create more; the RS is what
#    has to go, and by name rather than by label — the new pods deliberately
#    carry the same `app:` label as the old ones.
kubectl -n fastllm get rs -l app=fastllm-proxy -o name > /tmp/old-rs
kubectl -n fastllm get rs -l app=fastllm-control -o name >> /tmp/old-rs

# 3. Orphan the old Deployments. The pods keep running and keep serving —
#    they still carry `app: fastllm-*`, so both VIPs still resolve to them.
kubectl -n fastllm delete deploy fastllm-proxy fastllm-control --cascade=orphan

# 4. Hand it to the operator. `pod.labels` in fastllmproxy.yaml is what makes
#    the new pods answer on the same Services as the orphaned ones, so the
#    two sets serve side by side while the new ones come up.
kubectl apply -f deploy/fastllmproxy.yaml
kubectl -n fastllm rollout status deploy/fastllm-proxy

# 5. Once the new pods are Ready, retire the orphans.
xargs kubectl -n fastllm delete < /tmp/old-rs
```

Nothing in Postgres is touched by any of this: models, keys, grants, routing
rules and usage are the control plane's, not the operator's, and the resource
only ever names the Secrets the control plane already read. Verified by
comparing before and after: same six models on both gateway addresses, the
same eight backends all healthy, and `models=7 backends=8 keys=18
principals=7 rules=2 vmodels=1` on both sides of the cutover.

Two things worth knowing before doing this again:

- **Pre-label the running pods** with the operator's selector labels
  (`app.kubernetes.io/name`, `instance`, `component`) _before_ applying the
  resource. The Service selector flips to those labels the moment the
  operator reconciles, and without them the VIP has no endpoints until the
  new pods pass readiness.
- **Do not copy `serviceAnnotations` into a scratch namespace** to test a
  render. kube-vip honours them wherever they appear, and a second Service
  claiming a production VIP is a bad way to find that out.
- **Expect a few seconds of external unreachability per VIP.** Updating a
  Service makes kube-vip re-announce the address, so `.125` and `.126` blink
  from outside the cluster while ARP settles — in-cluster traffic and the
  pods themselves are unaffected throughout. It looks alarming and is not:
  during this cutover the same addresses answered 200, then nothing, then 200
  again within a few seconds, twice.

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
# providers.upstream_api_key with this before writing it to Postgres
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

Keys are minted through the control plane's admin API at `https://192.168.10.129:4001` (see the section above for what that address is and what guards it). Log in first (see "Bootstrapping the first admin login" above for the very first one) to get a session cookie, then reuse it:

```bash
kubectl -n fastllm exec deploy/fastllm-control -- sh -c '
  curl -s --cacert /etc/fastllm/tls/ca.crt -c /tmp/cookie -XPOST https://localhost:4001/login \
    -H "content-type: application/json" -d "{\"name\":\"admin\",\"password\":\"$ADMIN_PASSWORD\"}" \
  && curl -s --cacert /etc/fastllm/tls/ca.crt -b /tmp/cookie -XPOST https://localhost:4001/admin/keys \
    -H "content-type: application/json" -d "{\"name\":\"my-client\",\"principal_id\":1}"
'
# {"id":7,"key":"sk-..."}
```

Or open the management UI at **`https://192.168.10.129:4001/`** instead of hand-writing `curl` — same admin API underneath, a form instead of JSON. No port-forward: that address is a pinned VIP. Your browser will warn once about the private CA (see above).

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
against `providers`/`provider_models` is no longer the documented way to do any of
this; it bypasses the write path and waits on the periodic rebuild.

```bash
E() { kubectl -n fastllm exec deploy/fastllm-control -- "$@"; }

E curl -s --cacert /etc/fastllm/tls/ca.crt https://localhost:4001/admin/provider-models     # models, backend ids, api_bases

E curl -s --cacert /etc/fastllm/tls/ca.crt -XPOST https://localhost:4001/admin/provider-models -H 'content-type: application/json' \
  -d '{"name":"qwen3-6-35b-a3b-nvfp4"}'
# {"id":3,"name":"qwen3-6-35b-a3b-nvfp4"}

# Add a backend to that pool. upstream_model defaults to the model's own name;
# upstream_api_key is encrypted before it reaches Postgres and can never be
# read back — GET /admin/provider-models reports only whether one is set.
E curl -s --cacert /etc/fastllm/tls/ca.crt -XPOST https://localhost:4001/admin/provider-models/3/backends -H 'content-type: application/json' \
  -d '{"api_base":"http://192.168.10.245:40045/v1"}'
# {"id":9,...}

E curl -s --cacert /etc/fastllm/tls/ca.crt -XDELETE https://localhost:4001/admin/backends/9   # drop one replica
E curl -s --cacert /etc/fastllm/tls/ca.crt -XDELETE https://localhost:4001/admin/provider-models/3     # drop the model and its backends
```

## Memory, and the classifier

Both classifier models ship in the image, and the process that loads them needs
room for them:

|               | without classes | fast tier | both tiers |
| ------------- | --------------- | --------- | ---------- |
| proxy         | 85 Mi           | ~150 Mi   | 268 Mi     |
| control plane | ~90 Mi          | 272 Mi    | 275 Mi     |

Measured on this cluster. The manifests request 384 Mi and limit 1 Gi for the
control plane, 128 Mi / 1 Gi for a proxy. A limit below ~350 Mi will OOM-kill
the process the moment a prompt class is defined — which is exactly what the
old 256 Mi limit did, because it was set before the feature existed.

Nothing loads the refined tier unless a routing rule names a class that needs
it, so a deployment using only fast-tier classes stays at the middle column.

## Adding a provider

Most providers need no image change and no restart — a backend row is the
whole of it, and the proxy picks it up on the next snapshot rebuild. See
`README.md`'s provider table for verified base URLs. OpenRouter is the
shortest path to Anthropic and Gemini models, because it speaks OpenAI format
and needs no protocol setting:

```bash
# The cluster CA, so the connection verifies rather than being waved through
# with -k. Fetch once; it outlives any single certificate.
kubectl -n fastllm get secret fastllm-control-tls -o jsonpath='{.data.ca\.crt}' \
  | base64 -d > /tmp/kwca.crt
C="curl -s --cacert /tmp/kwca.crt"

$C -c /tmp/ck -X POST https://192.168.10.129:4001/login \
  -H 'content-type: application/json' \
  -d '{"name":"bootstrap","password":"..."}'
$C -b /tmp/ck -X POST https://192.168.10.129:4001/admin/provider-models \
  -H 'content-type: application/json' -d '{"name":"claude-sonnet","description":"via OpenRouter"}'
$C -b /tmp/ck -X POST https://192.168.10.129:4001/admin/provider-models/$ID/backends \
  -H 'content-type: application/json' \
  -d '{"api_base":"https://openrouter.ai/api/v1",
       "upstream_model":"anthropic/claude-sonnet-4",
       "upstream_api_key":"sk-or-..."}'
```

Bedrock and Cohere are ordinary rows: both speak OpenAI format with a bearer
key, and Bedrock needs no SigV4 signing. Vertex AI takes the service account's
JSON key file plus `"credential_kind":"gcp_service_account"` — the control
plane mints and refreshes the access token, so the proxy pods need no Google
credentials and no egress to `oauth2.googleapis.com`; the control plane does.

Reaching Anthropic or Gemini directly means adding `"protocol":"anthropic"`
or `"protocol":"gemini"`. Two operational notes:

- Anthropic backends want `"default_max_tokens": 4096` (or whatever suits).
  Without it, any client request that omits `max_tokens` gets a 400 — the
  provider requires the field and the proxy will not invent a cap.
- Translated backends serve `/chat/completions` only. Text, tool calling and
  image/audio input all work, streaming included; the embeddings/audio
  endpoints return 501. Use an OpenRouter backend for those.

The control plane needs egress to the provider and its CA in the trust store;
public roots are already present in the image, so the hosted providers work
without `--ca-bundle`.

## Letting hosts register themselves

The section below is the manual version of this, and it is the drift it
describes that motivated the alternative: run
[`agent/fastllm-node-agent.py`](../agent/fastllm-node-agent.py) on the host and
it registers its own addresses on a lease, so a moved port corrects itself.

The control plane then probes each provider on `--provider-sweep-interval`
(60s), which answers two questions with one call: whether it is reachable, and
whether it is still serving what is registered against it. The second is the
one that bit this cluster — a Spark answering happily while serving a different
model than the row claimed, which a health check reports as healthy.

A dynamic provider that stops answering degrades first and is deleted only
after 30 minutes, which is longer than a 27B takes to load. Static and cloud
providers are probed too but never expire.

See [Registering hosts that serve models](../docs/operations/registering-hosts.md).

## When a Spark's port changes

Do this when nothing is registering the host for you. GPUStack assigns the
replica port and it moves on redeploy. If the backend goes
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

E curl -s --cacert /etc/fastllm/tls/ca.crt https://localhost:4001/admin/provider-models          # find the model id and the stale backend id
E curl -s --cacert /etc/fastllm/tls/ca.crt -XDELETE https://localhost:4001/admin/backends/9
E curl -s --cacert /etc/fastllm/tls/ca.crt -XPOST https://localhost:4001/admin/provider-models/3/backends -H 'content-type: application/json' \
  -d '{"api_base":"http://192.168.10.245:40118/v1"}'
```

Both writes publish a rebuilt snapshot immediately, so this lands on
`fastllm-proxy` within one `--config-poll` interval (default 5s). Re-running
`import` against an updated config file also works and is the better choice
when several backends moved at once, at the cost of waiting for
`--snapshot-rebuild-interval` as well. This is the main thing a discovery
source would automate later.
