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
NAME      PHASE   GATEWAY   CONTROL   IMAGE                                   AGE
fastllm   Ready   3/3       true      ghcr.io/azrtydxb/fastllm-proxy:v0.2.0   2m
```

`IMAGE` is what is **actually serving**, not what was asked for. During an
upgrade it lags `spec.image` on purpose — see below.

A full example, including TLS and the tuning block, is in
[`deploy/example.yaml`](deploy/example.yaml).

## Why this rather than the chart

The chart and the [manifests](../deploy/kubernetes) describe a deployment
once, at apply time. What neither can do is *keep* describing it. A Deployment
edited by hand stays edited. A proxy pinned to an older image than its control
plane stays pinned — and that one is not cosmetic: the two share a database
schema, so the older side reads a snapshot carrying fields it does not
understand.

Five properties fall out of a controller that a template cannot give you:

- **Both planes always run the same image, in the right order.** `spec.image`
  is one field and becomes both Deployments, and the control plane rolls
  *first*: the gateway is held at the image it is currently running until the
  control plane is fully rolled and ready. An image that cannot be pulled
  therefore takes the control plane down and leaves the gateway serving from
  its cached snapshot, instead of breaking both at once.
- **A rotated Secret rolls the pods that read it.** Env from a `secretKeyRef`
  is resolved once, at container start; rewriting the Secret afterwards
  changes nothing until something restarts the pod. Both pod templates carry
  a hash of the resolved material, so rotating the proxy token — or
  cert-manager renewing the control-plane certificate — *is* the rollout.
- **A broken configuration is refused rather than deployed.** Every Secret is
  resolved and checked first. A missing key, or an encryption key that is not
  32 bytes of hex, becomes a condition naming the Secret and the key. Nothing
  is applied on that pass, so an existing healthy deployment is never rolled
  onto a configuration already known to be bad.
- **The install finishes.** With `bootstrap` set, a Job runs `set-password`
  once the control plane is ready, so you end with a UI somebody can sign
  into rather than one nobody can.
- **Scaling means scaling the data plane.** `proxy.replicas`, or an HPA via
  `proxy.autoscaling` — and while that is on, the controller stops writing
  the replica count so the two do not fight. The control plane is not exposed
  as a replica count at all, because a second one only races the first
  rebuilding snapshots.

Use the chart if you want values and templating across many environments; use
the manifests if you want to read exactly what is applied. They are all the
same two Deployments.

## What it does not do

**It does not run your database.** A `FastllmProxy` points at a Secret holding
a connection string. A database is a stateful service with backup, failover
and upgrade concerns that belong to whoever operates it, and an operator that
quietly owns one is an operator that can quietly lose one.

**It cannot write Secrets.** It reads the ones a `FastllmProxy` names — it
has to, to say *which* one is missing a key and to notice a rotation — and
the RBAC grants `get`/`list`/`watch` and nothing else. A controller that could
mint credentials the cluster then trusts is a much larger blast radius for no
gain. Resolved material is hashed, never logged and never echoed into the
status: a status subresource is readable by anyone who can `get` the CR, which
is a far wider set than "can read the Secret".

**It does not manage models, keys or routing rules.** Those live in the
control plane's database and change through the admin API or the UI, without
a rollout. Expressing them as CRDs would mean two sources of truth for the
same rows.

## The management UI knows about it

Under an operator, the UI grows one screen — **Deployment** — and it is the
only screen that edits a Kubernetes resource rather than a database row:
image, replicas, policy, upstream timeout, workers, pool size and autoscaling,
with the phase, the conditions, the config hash and what is *actually*
serving. Applying a change patches the `FastllmProxy`; the operator does the
rest, so the page says the rollout is the operator's job rather than reporting
"saved" and looking finished.

It is **absent without an operator**. The control plane learns it is managed
from `FASTLLM_OPERATOR_RESOURCE`, which only this controller injects, so a
Helm or manifest install has no such screen in its navigation and
`GET /admin/deployment` answers 404 there. Nothing is greyed out, because
there is nothing to enable.

The control plane reaches the API server with its own ServiceAccount and a
namespaced Role naming exactly one `resourceName` — `get` and `patch` on the
`FastllmProxy` it belongs to, and nothing else in the cluster. Secret
references, `serviceType`, ingress and `bootstrap` are deliberately not
editable from a web form: the first would lose every stored upstream
credential, the middle two are network decisions that should leave a trail
where the cluster is managed, and the last would reset an admin password from
a page that password protects.

## The fields

| | |
|---|---|
| `image` | The image **both** planes run |
| `imagePullPolicy` / `imagePullSecrets` | For a private registry |
| `database` / `proxyToken` / `encryptionKey` | `{name, key}` Secret references. All three required. `encryptionKey` is **immutable**, enforced by the API server |
| `bootstrap.name` / `bootstrap.password` | The first admin login, created once by a Job |
| `observability.logLevel` / `logFormat` | `FASTLLM_LOG`, and `text` or `json` |
| `observability.otlpEndpoint` / `otlpSampleOneIn` | OTLP/gRPC tracing, for an image built with the `otel` feature |
| `observability.serviceMonitor` | `enabled`, `interval`, `labels`. Skipped without complaint where the Prometheus operator is not installed |
| `control.tlsSecretName` | Turns on TLS for the admin listener — and moves the probes and the gateway's `--ca-bundle` with it |
| `control.resources` / `control.serviceAnnotations` | |
| `control.serviceType` | `ClusterIP` by default. Anything else is **refused by the API server** without `tlsSecretName` — this Service fronts `/snapshot`, which returns decrypted upstream credentials |
| `proxy.replicas` | Gateway replicas. Below 2, no PodDisruptionBudget is created |
| `proxy.autoscaling` | `enabled`, `minReplicas`, `maxReplicas`, `targetCpuUtilizationPercentage`. While enabled, `replicas` is left to the HPA |
| `proxy.policy` | `cacheAffinity` (default), `leastLoaded`, `roundRobin`, `lowestLatency` |
| `proxy.upstreamTimeout` | Seconds to wait for response *headers* |
| `proxy.workers` / `proxy.poolMaxIdle` | The two knobs that matter under load — see [docs/performance.md](../docs/performance.md) |
| `proxy.serviceType` / `proxy.serviceAnnotations` | For the **gateway** only. The annotations are where a pinned load-balancer address goes |
| `proxy.servicePorts` | Every address the gateway answers on — `:80` **and** `:4000` is the common pair. Empty means one port, 4000 |
| `proxy.ingress` | `enabled`, `className`, `host`, `path`, `annotations`, `tlsSecretName` |
| `proxy.classifier` | Tier-1 and tier-2 model directories for semantic routing |
| `proxy.resources` | Compute resources for the gateway container |
| `*.pod` | `nodeSelector`, `tolerations`, `affinity`, `priorityClassName`, `annotations`, `labels`, `extraArgs`, `extraEnv` — on either plane |
| `tuning` | The `fastllm:` block, verbatim. Editing it rolls the gateway |

`extraArgs` is appended after every flag the controller computes, so it is
also the escape hatch for anything not modelled here: an unmodelled flag
should never mean abandoning the operator.

## Reading the status

| | |
|---|---|
| `phase` | `Pending`, `Upgrading`, `Bootstrapping`, `Degraded`, `Ready` |
| `observedImage` | What is actually serving |
| `configHash` | The hash the pods were last rendered with — "did the rotation take?" without diffing pod templates |
| `bootstrapped` | Whether the admin login exists. Never reset by the controller: re-running `set-password` is a password reset, and doing that on its own would lock an operator out |
| `conditions` | `Ready`, `SecretsResolved`, `Upgrading`, `Bootstrapped` |

A gateway reports unready until at least one model **backend** is healthy, so
a brand-new install sits in `Degraded` until a model is added. The `Ready`
condition says so rather than leaving you to find out.

`control.replicas` is absent on purpose. The admin Service defaults to
ClusterIP because it fronts `/snapshot`, which returns *decrypted* upstream
credentials to anything holding the proxy token — but exposing it deliberately,
TLS-only, on its own address is a real deployment
([deploy/README.md](../deploy/README.md) runs exactly that), so it is a field
whose unsafe value the CRD refuses rather than a decision the schema pretends
nobody makes. Exposing it still rests on the preconditions in
[docs/security.md](../docs/security.md).

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
first. Outside a pod it takes leadership under a `local-<pid>` identity and
looks for the Lease in `fastllm-system`.

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

**RBAC is only real against an API server.** The event recorder writes through
`events.k8s.io/v1`; the ClusterRole granted only the core group's `events`.
Every unit test passed and every event was refused with a 403 that appeared
nowhere but the controller's own log. It was found by running the thing
against a live cluster, which is the only place that class of bug exists.

**A derived `Default` is not the schema's default.** serde fills an omitted
field from `#[serde(default = "...")]`, but `#[derive(Default)]` fills it with
`0`/`false`/`""` — so `AutoscalingSpec::default()` meant a floor of *zero*
replicas, and the generated CRD advertised `minReplicas: 0` to anyone reading
it. Both defaults are now written once and asserted equal by
`spec_defaults_match_the_schema_defaults`.

**Leadership is a Lease, not `replicas: 1`.** Pinning the Deployment to one
replica prevents two writers by removing availability: a node drain leaves the
cluster with no controller at all. Every replica now runs, one holds
`fastllm-operator-leader`, and a replica that loses it exits rather than
carrying on with a claim it no longer has.
