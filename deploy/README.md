# Deploying to the kw cluster

Plain manifests — one Deployment does not earn a Helm chart.

| | |
|---|---|
| Namespace | `fastllm` |
| Image | `192.168.10.123:5000/azrtydxb/fastllm-proxy/proxy:main` (zot, anonymous pull) |
| VIP | `192.168.10.126` via kube-vip |
| Backend | spark2 `192.168.10.245:40045/v1`, model `qwen3-6-35b-a3b-nvfp4` |

## First install

The master key is deliberately not in git. Generate one and create the Secret:

```bash
kubectl create namespace fastllm --dry-run=client -o yaml | kubectl apply -f -

kubectl -n fastllm create secret generic fastllm-proxy-auth \
  --from-literal=master-key="sk-$(openssl rand -hex 24)"

kubectl apply -f deploy/
```

Read the key back when you need it for a client:

```bash
kubectl -n fastllm get secret fastllm-proxy-auth \
  -o jsonpath='{.data.master-key}' | base64 -d
```

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

Edit `configmap.yaml`, `kubectl apply -f deploy/configmap.yaml`, and wait.
No rollout is needed: the proxy hashes the mounted file every 5s and reloads
in place, so generations in flight survive the change. Budget up to ~60s for
the kubelet to refresh the mount, then one poll interval.

An edit that leaves the file invalid is logged once and ignored — the previous
config keeps serving.

## When spark2's port changes

GPUStack assigns the replica port and it moves on redeploy. If the backend goes
unhealthy, find the current one:

```bash
curl -H "Authorization: Bearer $GPUSTACK_KEY" \
  http://192.168.10.125/v2/model-instances
```

then update `api_base` in the ConfigMap. This is the main thing a discovery
source would automate later.
