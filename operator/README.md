# Kubernetes operator

`kubectl get fastllmproxy`, and a deployment that stays the shape you asked
for.

```bash
kubectl apply -f operator/deploy/crd.yaml
kubectl apply -f operator/deploy/operator.yaml
kubectl apply -f operator/deploy/rbac.yaml
```

Then one resource per deployment:

```yaml
apiVersion: fastllm.io/v1alpha1
kind: FastllmProxy
metadata:
  name: fastllm
  namespace: fastllm
spec:
  image: ghcr.io/azrtydxb/fastllm-proxy:v0.2.0
  database:      { name: fastllm-database, key: uri }
  proxyToken:    { name: fastllm-secrets,  key: proxy-token }
  encryptionKey: { name: fastllm-secrets,  key: encryption-key }
  proxy:
    replicas: 3
    policy: cacheAffinity
```

```console
$ kubectl -n fastllm get fllm
NAME      GATEWAY   CONTROL   IMAGE                                   AGE
fastllm   3/3       true      ghcr.io/azrtydxb/fastllm-proxy:v0.2.0   2m
```

A full example, including TLS and the tuning block, is in
[`deploy/example.yaml`](deploy/example.yaml).

## Why this rather than the chart

The chart and the [manifests](../deploy/kubernetes) describe a deployment
once, at apply time. What neither can do is *keep* describing it. A Deployment
edited by hand stays edited. A proxy pinned to an older image than its control
plane stays pinned — and that one is not cosmetic: the two share a database
schema, so the older side reads a snapshot carrying fields it does not
understand.

Two properties fall out of a controller that a template cannot give you:

- **Both planes always run the same image.** `spec.image` is one field and
  becomes both Deployments. Pinning them apart is not something the API lets
  you express.
- **Scaling means scaling the data plane.** `proxy.replicas` is a number; the
  control plane is not exposed as one, because a second control plane only
  races the first rebuilding snapshots.

Use the chart if you want values and templating across many environments; use
the manifests if you want to read exactly what is applied. They are all the
same two Deployments.

## What it does not do

**It does not run your database.** A `FastllmProxy` points at a Secret holding
a connection string. A database is a stateful service with backup, failover
and upgrade concerns that belong to whoever operates it, and an operator that
quietly owns one is an operator that can quietly lose one.

**It cannot read Secrets.** The RBAC does not grant it. The controller names
Secrets in a pod spec and the kubelet resolves them; a controller that could
read every Secret in the cluster is a much larger blast radius for no gain.

**It does not manage models, keys or routing rules.** Those live in the
control plane's database and change through the admin API or the UI, without
a rollout. Expressing them as CRDs would mean two sources of truth for the
same rows.

## The fields

| | |
|---|---|
| `image` | The image **both** planes run |
| `database` / `proxyToken` / `encryptionKey` | `{name, key}` Secret references. All three required |
| `control.tlsSecretName` | Turns on TLS for the admin listener — and moves the probes and the gateway's `--ca-bundle` with it |
| `control.resources` | Compute resources for the control-plane container |
| `proxy.replicas` | Gateway replicas. Below 2, no PodDisruptionBudget is created |
| `proxy.policy` | `cacheAffinity` (default), `leastLoaded`, `roundRobin`, `lowestLatency` |
| `proxy.upstreamTimeout` | Seconds to wait for response *headers* |
| `proxy.serviceType` | `ClusterIP` (default), `LoadBalancer`, `NodePort` — for the **gateway** only |
| `proxy.resources` | Compute resources for the gateway container |
| `tuning` | The `fastllm:` block, verbatim |

`control.replicas` is absent on purpose, and the admin Service is always
ClusterIP: it fronts `/snapshot`, which returns *decrypted* upstream
credentials to anything holding the proxy token. Exposing that is a decision
with three preconditions ([docs/security.md](../docs/security.md)), not a
field with a default.

## Developing

```bash
cargo test -p fastllm-operator
cargo run -p fastllm-operator --bin crdgen > operator/deploy/crd.yaml
```

The CRD is generated from the same Rust types the controller reconciles, and
`tests/crd_is_current.rs` fails if the committed YAML has drifted. Fix a
failure by regenerating, never by editing the YAML.

Run it against your current kubeconfig with `cargo run -p fastllm-operator`.
It reconciles in whatever cluster `kubectl` is pointed at, so check that
first.

## Notes from building it

**The status write is a change to the resource**, so the watch fires and the
controller reconciles again. Stamping `lastTransitionTime` every pass meant
the status was never equal to itself: measured at roughly ten reconciles a
second against an idle cluster. The controller now carries the previous
timestamp forward while the condition has not changed, and writes only on a
real difference — which is also what makes `lastTransitionTime` mean what its
name says.

**Server-side apply, with a fixed field manager.** The API server diffs
against the fields this controller owns and leaves everything else alone, so
an annotation added by a service mesh survives a reconcile. A
read-modify-write loop would fight those tools for ever.

**Selector labels carry nothing that varies between releases.**
`spec.selector` is immutable on a Deployment, so a version label in there
would make every upgrade a delete and recreate.
