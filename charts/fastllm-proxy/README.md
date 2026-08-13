# fastllm-proxy Helm chart

```bash
helm install gw ./charts/fastllm-proxy \
  --set image.tag=v0.1.0 \
  --set database.url="postgres://user:pass@postgres:5432/fastllm"
```

Two values have no sensible default: the image tag and a database. Everything
else has one, and every non-obvious default carries the reason in
`values.yaml`.

## What it does not do

**It does not run Postgres.** A database has backup, upgrade and failover
concerns that belong to whoever operates it. Point `database.url` at your own,
or at an operator-managed cluster (CloudNativePG, CrunchyData, RDS).

**It does not create Ingress.** The two listeners want different exposure —
the gateway is for callers, the admin port serves the management UI *and*
`/snapshot`, which returns decrypted upstream credentials to anything holding
the proxy token. Bundling them behind one Ingress is the mistake this chart
declines to make for you. Set `service.type` per component and route them
yourself.

## Secrets

Two are generated on first install and **preserved across upgrades** by
looking up the existing Secret. That preservation is not a nicety:
regenerating `encryption-key` would leave every stored upstream credential
undecryptable — the rows remain and nothing can read them. Rotating it for
real means running `fastllm-proxy reencrypt-backends` first.

Supply your own with `secrets.existingSecret` if you would rather manage them
outside Helm.

## Pin the image

`image.tag` defaults to the chart's `appVersion`. Pin a digest or a release
tag, never a floating one. A drifting tag is how a control plane silently
downgraded itself here: `sqlx` refuses to run against a schema newer than the
binary — correctly — and the failure lands at startup, after the deploy
reported success.

## Scaling

Scale `proxy.replicas`. The control plane stays at one: it is not on the
request path, and a second would race the first rebuilding snapshots for no
gain.

Note that the response cache is per process, so a repeated request only hits
if it lands on the replica that served it before. That is deliberate — see
`TODO.md` for why a shared cache was ruled out rather than deferred.
