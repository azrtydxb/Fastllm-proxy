#!/usr/bin/env python3
"""Register the model endpoints on this host with FastLLM, and keep saying so.

Runs on a machine that serves models -- a DGX Spark, a Docker host, a node in
some other cluster. It dials the control plane and is never dialled, so it
works from behind NAT or on a cluster FastLLM cannot reach into.

It registers *addresses*, not models. The control plane calls `GET /v1/models`
itself, because FastLLM has to reach the endpoint anyway in order to serve
traffic: a model list pushed from here could name models the proxies cannot
dial, and that failure would surface at request time, to a user. Letting the
control plane enumerate makes discovery and reachability the same test.

Standard library only, on purpose. This runs on machines whose Python is
whatever the vendor shipped, and a health agent that needs a virtualenv to
start is one more thing to be broken at 3am.
"""

import argparse
import json
import os
import socket
import ssl
import sys
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

# Every engine worth naming answers this: vLLM, SGLang, llama.cpp's server,
# TGI, Ollama, Triton's OpenAI frontend, LM Studio, mlx-lm. That is why this
# agent never needs to know which one it found -- an unrecognised engine is
# registered like any other, it just contributes no metadata.
#
# Appended to an `api_base` that already ends in `/v1`, which is the form
# FastLLM stores and the form every engine documents.
MODELS_PATH = "/models"


def log(msg):
    print(f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} {msg}", flush=True)


def serves_models(base, timeout):
    """True when something at `base` answers the one call that matters.

    The model ids it answers with are kept in MODEL_OF: the provider is named
    after what it serves, and this is the one place that is learned.
    """
    try:
        req = urllib.request.Request(base.rstrip("/") + MODELS_PATH)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            body = json.load(r)
        data = body.get("data")
        if not isinstance(data, list):
            return False
        ids = [m.get("id") for m in data if isinstance(m, dict) and m.get("id")]
        # Overwritten on every answer: an engine that stops listing a model
        # must not keep naming its provider after it.
        if ids:
            MODEL_OF[base] = ids[0]
        else:
            MODEL_OF.pop(base, None)
        return True
    except Exception:
        return False


SA_DIR = "/var/run/secrets/kubernetes.io/serviceaccount"


def kube_get(path, timeout):
    """GET a Kubernetes API path using the pod's own ServiceAccount.

    Standard library, like everything else here: the token and the CA are
    files the kubelet already mounted, and the API speaks JSON over HTTPS. A
    client library would buy nothing and cost the one property this agent is
    built on -- that it runs wherever Python does, with nothing installed.
    """
    with open(f"{SA_DIR}/token", encoding="utf-8") as f:
        token = f.read().strip()
    host = os.environ.get("KUBERNETES_SERVICE_HOST", "kubernetes.default.svc")
    port = os.environ.get("KUBERNETES_SERVICE_PORT", "443")
    req = urllib.request.Request(
        f"https://{host}:{port}{path}",
        headers={"authorization": f"Bearer {token}"},
    )
    ctx = ssl.create_default_context(cafile=f"{SA_DIR}/ca.crt")
    with urllib.request.urlopen(req, timeout=timeout, context=ctx) as r:
        return json.load(r)


def node_addresses(args, timeout):
    """Node name -> the address a proxy should dial for pods on that node.

    A hostNetwork pod listens on its *node*, so the honest address carries
    the node's IP -- and a cluster runs engines on several nodes, where one
    --advertise would register half of them under the wrong host. The
    InternalIP is the address everything else on the wire already uses.
    --advertise stays the override for clusters whose nodes are reached
    some other way.
    """
    try:
        items = kube_get("/api/v1/nodes", timeout).get("items", [])
    except Exception as e:
        log(f"could not list nodes: {e}")
        return {}
    out = {}
    for node in items:
        name = (node.get("metadata") or {}).get("name")
        for addr in (node.get("status") or {}).get("addresses") or []:
            if addr.get("type") == "InternalIP" and addr.get("address"):
                out[name] = addr["address"]
                break
    return out


def port_from_command(container):
    """The port a host-network container says it listens on, from its command.

    A pod on `hostNetwork` publishes on the node whether or not it declares a
    `containerPort`, and a pod that declares none tells the API nothing about
    where it listens -- which is how a working engine stays invisible to a
    scan that only reads port fields. This deployment's own engines are
    exactly that: `hostNetwork: true`, no ports declared, serving on 8000.

    So read the flag the operator already wrote. `--port N` and `--port=N` are
    what vLLM, SGLang and llama.cpp's server all take, and this is reading a
    declaration rather than guessing at a range -- the distinction that makes
    it not a port scan.

    The better fix is upstream of here: a `containerPort` on a host-network
    pod costs nothing and makes the API describe it properly. This covers the
    pods that do not.
    """
    argv = (container.get("command") or []) + (container.get("args") or [])
    out = []
    for i, tok in enumerate(argv):
        if tok == "--port" and i + 1 < len(argv):
            candidate = argv[i + 1]
        elif tok.startswith("--port="):
            candidate = tok.split("=", 1)[1]
        else:
            continue
        try:
            out.append(int(candidate))
        except ValueError:
            pass
    return out


def port_from_probes(container):
    """The port a container's own health probes say it serves HTTP on.

    The last declaration to read, for an engine that takes its port from a
    config file rather than a flag -- the audio.cpp servers on kw do, and an
    `httpGet` readiness probe is the only place their pod spec names the port.
    Like `--port`, this reads what the operator already wrote rather than
    guessing. A named port refers to a declared `containerPort`, which would
    have been read already, so only numbers count here.
    """
    out = []
    for kind in ("readinessProbe", "livenessProbe", "startupProbe"):
        port = ((container.get(kind) or {}).get("httpGet") or {}).get("port")
        if isinstance(port, int):
            out.append(port)
        elif isinstance(port, str) and port.isdigit():
            out.append(int(port))
    return list(dict.fromkeys(out))


# Which node each Kubernetes candidate runs on, and which model each endpoint
# serves, so every provider name is <cluster>-<node>-<model>-<port>: two
# endpoints on one port, one node or one model still get different names.
# Written by the discovery thread, read by the heartbeat; each assignment is
# one dict store.
LABEL_OF = {}
MODEL_OF = {}


def service_node(svc, pods):
    """The node a Service's endpoints run on, from its selector.

    A Service has no node; the pods it selects do. When they all run on one
    node that node names the provider; spread over several, none is the
    answer and the name carries no node rather than a misleading one.
    """
    selector = (svc.get("spec") or {}).get("selector") or {}
    namespace = (svc.get("metadata") or {}).get("namespace")
    if not selector:
        return None
    nodes = set()
    for pod in pods:
        meta = pod.get("metadata") or {}
        labels = meta.get("labels") or {}
        if meta.get("namespace") != namespace:
            continue
        if all(labels.get(k) == v for k, v in selector.items()):
            node = (pod.get("spec") or {}).get("nodeName")
            if node:
                nodes.add(node)
    return nodes.pop() if len(nodes) == 1 else None


def kube_candidates(args):
    """Addresses this cluster actually exposes, from the API rather than a guess.

    A port probe is what you do on a host with no service registry. Kubernetes
    has one, and it knows exactly which ports are reachable from outside --
    so ask it, and probe only those.

    *Exposed* is the whole criterion, and it is why this needs no label or
    annotation to opt in. A NodePort, a LoadBalancer and a hostPort are
    reachable from outside the cluster by definition; a ClusterIP is not. So a
    ClusterIP-only Service is never a candidate -- not refused, simply not
    exposed, which is the same answer the operator already gave by choosing
    that type.

    That matters because what gets registered is a destination for *someone
    else's* traffic. This agent dials out, but a proxy elsewhere later dials
    the address it registered. Registering an unreachable one succeeds here
    and fails at request time, to a user -- the same trap `--advertise` exists
    to avoid on a bare host.

    Returns candidate `api_base` URLs. Whether any of them serves models is
    still decided by the `/v1/models` probe every other source goes through,
    so discovery and reachability stay the same test.
    """
    found = []

    # Pods first: a Service is named after the node its pods run on.
    try:
        pods = kube_get("/api/v1/pods", args.probe_timeout).get("items", [])
    except Exception as e:
        log(f"could not list pods: {e}")
        pods = []

    try:
        services = kube_get("/api/v1/services", args.probe_timeout).get("items", [])
    except Exception as e:
        log(f"could not list services: {e}")
        services = []
    for svc in services:
        spec = svc.get("spec") or {}
        kind = spec.get("type")
        meta = svc.get("metadata") or {}
        # An explicit address wins over anything inferred, for the Service
        # whose reachable address this agent would get wrong.
        override = (meta.get("annotations") or {}).get("fastllm.io/advertise")
        node = service_node(svc, pods)
        for port in spec.get("ports") or []:
            if override:
                url = f"http://{override}/v1" if "://" not in override else override
                # Only http(s): urllib also reads file:// paths, and anyone who
                # can annotate a Service would otherwise choose what the
                # agent opens.
                if not url.lower().startswith(("http://", "https://")):
                    log(
                        f"ignoring fastllm.io/advertise {override!r}: not an http(s) URL"
                    )
                    continue
                found.append(url)
                LABEL_OF[url] = node
                continue
            if kind == "LoadBalancer":
                for ing in ((svc.get("status") or {}).get("loadBalancer") or {}).get(
                    "ingress", []
                ):
                    addr = ing.get("ip") or ing.get("hostname")
                    if addr and port.get("port"):
                        url = f"http://{addr}:{port['port']}/v1"
                        found.append(url)
                        LABEL_OF[url] = node or meta.get("name")
            elif kind == "NodePort" and port.get("nodePort") and args.advertise:
                url = f"http://{args.advertise}:{port['nodePort']}/v1"
                found.append(url)
                LABEL_OF[url] = node

    # A hostPort or host networking bypasses Services entirely and is a normal
    # way to expose a single-node engine, so both would be invisible to a
    # Service-only scan.
    node_ips = node_addresses(args, args.probe_timeout)
    for pod in pods:
        spec = pod.get("spec") or {}
        host_net = bool(spec.get("hostNetwork"))
        # The pod's own node first, --advertise as the override.
        advertise = (
            node_ips.get((pod.get("spec") or {}).get("nodeName")) or args.advertise
        )
        for c in spec.get("containers") or []:
            ports = [p["hostPort"] for p in (c.get("ports") or []) if p.get("hostPort")]
            # Under host networking every containerPort *is* a node port, so a
            # pod that declares one has already said where it listens.
            if host_net:
                ports += [
                    p["containerPort"]
                    for p in (c.get("ports") or [])
                    if p.get("containerPort")
                ]
                if not ports:
                    ports += port_from_command(c)
                if not ports:
                    ports += port_from_probes(c)
            for hp in ports:
                if advertise:
                    url = f"http://{advertise}:{hp}/v1"
                    found.append(url)
                    LABEL_OF[url] = spec.get("nodeName")

    # Two Services can front the same endpoint; register it once.
    return list(dict.fromkeys(found))


def discover(args):
    """Endpoints on this host that serve models.

    Sources compose and are all optional, which is what makes "in Docker or
    not" fall out rather than being a mode. The port probe alone covers a bare
    process started by hand or by a launcher, with no container runtime
    present at all.

    Every candidate is probed at once rather than in turn. A cluster exposes
    hundreds of ports that are not models — on kw, about 330 between
    LoadBalancer ports and host-networked pods — and probed one at a time,
    each costing up to the probe timeout, a pass took over a quarter of an
    hour. Discovery that slow cannot be what keeps a 90-second lease alive;
    see `main` for how the two are kept apart.
    """
    candidates = list(args.api_base)
    if args.kubernetes:
        candidates += kube_candidates(args)
    # The advertised host, never a loopback or a container address: this is
    # the address the *proxies* will dial. An agent that discovers a container
    # on 172.17.0.2 and registers that hands the proxies an address they
    # cannot reach.
    candidates += [f"http://{args.advertise}:{port}/v1" for port in args.scan_ports]
    candidates = list(dict.fromkeys(candidates))
    if not candidates:
        return []

    with ThreadPoolExecutor(max_workers=args.probe_workers) as pool:
        answers = list(
            pool.map(lambda base: serves_models(base, args.probe_timeout), candidates)
        )
    for base, ok in zip(candidates, answers):
        if base in args.api_base and not ok:
            log(f"configured endpoint {base} did not answer {MODELS_PATH}")
    return [base for base, ok in zip(candidates, answers) if ok]


def tls_context(args):
    """How to verify the control plane, or None for plain HTTP.

    A control plane on a private network is very often behind a certificate
    from an internal CA -- this project's own dev cluster is -- and Python
    trusts the system store, which has never heard of it. Without a way to
    name that CA the agent cannot register at all, which is the state this
    was found in: discovery worked, every registration failed on
    CERTIFICATE_VERIFY_FAILED.

    The answer is a CA to trust, not a switch to stop checking. The bearer key
    this agent presents is a live credential on the wire, and an agent that
    skips verification hands it to whoever answers. A single self-signed
    certificate with no CA above it works here too: pass the certificate
    itself, since it is its own issuer.
    """
    if not args.control.lower().startswith("https://"):
        return None
    if args.ca_cert:
        return ssl.create_default_context(cafile=args.ca_cert)
    return ssl.create_default_context()


def name_part(value):
    """One component of a provider name: lowercase, [a-z0-9.] and dashes.

    A model id such as `nvidia/Qwen3.6-35B-A3B-NVFP4` carries a slash and
    capitals; the name is read on screens and in logs, so it is folded to
    `nvidia-qwen3.6-35b-a3b-nvfp4` rather than kept verbatim.
    """
    out = []
    for ch in str(value).lower():
        out.append(ch if ch.isascii() and (ch.isalnum() or ch == ".") else "-")
    return "-".join(p for p in "".join(out).split("-") if p)


def provider_name(args, api_base):
    """What to call this endpoint in FastLLM: <cluster>-<node>-<model>-<port>.

    The name lives here rather than on the control plane, which only ever sees
    an address. Each part answers a different question about the endpoint --
    which agent found it, which machine runs it, what it serves, where it
    listens -- and together they keep names apart where any one of them
    repeats: one agent speaks for a whole cluster, several engines share a
    port, one node serves several models, and one model runs on several
    nodes. A part this agent cannot know (no node for a bare host's own
    agent, no model before the first probe answers) is left out rather than
    guessed.

    The port is always appended, never only when a host happens to serve more
    than one endpoint: a name that changes shape as a second model is started
    would rename the first one behind the operator's back.
    """
    parts = [args.provider_name or args.node]
    # Under --kubernetes the agent is the cluster, so the node is a separate
    # part; on a bare host --node already is the machine.
    if args.kubernetes and LABEL_OF.get(api_base):
        parts.append(LABEL_OF[api_base])
    if MODEL_OF.get(api_base):
        parts.append(MODEL_OF[api_base])
    tail = api_base.split("://", 1)[-1].split("/")[0]
    if ":" in tail:
        parts.append(tail.rsplit(":", 1)[-1])
    return "-".join(p for p in (name_part(x) for x in parts) if p)


def register(args, api_base):
    body = json.dumps(
        {
            "api_base": api_base,
            "node": args.node,
            "name": provider_name(args, api_base),
            "engine": args.engine,
            "ttl_seconds": args.ttl,
        }
    ).encode()
    req = urllib.request.Request(
        args.control.rstrip("/") + "/admin/providers/register",
        data=body,
        method="POST",
        headers={
            "content-type": "application/json",
            "authorization": f"Bearer {args.token}",
        },
    )
    with urllib.request.urlopen(
        req, timeout=args.probe_timeout, context=tls_context(args)
    ) as r:
        return json.load(r)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--control",
        default=os.environ.get("FASTLLM_CONTROL_URL"),
        help="control plane admin base URL",
    )
    ap.add_argument(
        "--token",
        default=os.environ.get("FASTLLM_AGENT_TOKEN"),
        help="API key for this node's principal",
    )
    ap.add_argument(
        "--node",
        default=os.environ.get("FASTLLM_NODE", socket.gethostname()),
        help="name for this host, scoping what it may register",
    )
    ap.add_argument(
        "--advertise",
        default=os.environ.get("FASTLLM_ADVERTISE"),
        help="address the proxies should dial. Configured, never "
        "inferred: a discovered container address is one the "
        "proxies cannot reach",
    )
    ap.add_argument(
        "--api-base",
        action="append",
        default=[],
        help="an endpoint to register outright; repeatable",
    )
    ap.add_argument(
        "--kubernetes",
        action="store_true",
        default=os.environ.get("FASTLLM_KUBERNETES", "").lower()
        in ("1", "true", "yes"),
        help="Discover exposed endpoints from the Kubernetes API "
        "(NodePort, LoadBalancer, hostPort) instead of guessing "
        "at ports. Requires a ServiceAccount that can list "
        "services and pods.",
    )
    ap.add_argument(
        "--scan-ports",
        type=int,
        nargs="*",
        default=None,
        help="ports on --advertise to probe. Defaults to a short "
        "list, or to nothing under --kubernetes, where the "
        "API already knows what is exposed",
    )
    ap.add_argument(
        "--provider-name",
        default=os.environ.get("FASTLLM_PROVIDER_NAME"),
        help="what to call this host's providers in FastLLM; the "
        "endpoint's port is appended, so one host's endpoints "
        "are distinguishable. Defaults to --node. Sent on "
        "every heartbeat, so changing it renames them",
    )
    ap.add_argument(
        "--engine",
        default=os.environ.get("FASTLLM_ENGINE"),
        help="hint only; nothing depends on it",
    )
    ap.add_argument("--ttl", type=int, default=90, help="lease length in seconds")
    ap.add_argument(
        "--interval",
        type=int,
        default=30,
        help="how often to re-register. Well inside --ttl, so one "
        "missed beat is not an expiry",
    )
    ap.add_argument(
        "--ca-cert",
        default=os.environ.get("FASTLLM_CA_CERT"),
        help="PEM bundle to verify the control plane against, for "
        "a certificate from an internal CA. The CA's "
        "certificate, or a lone self-signed one, which is its "
        "own issuer. There is deliberately no way to skip "
        "verification: the token below goes over this "
        "connection",
    )
    ap.add_argument(
        "--discover-interval",
        type=int,
        default=60,
        help="how often to look for endpoints again. Independent of "
        "--interval: leases on what is already known are renewed on "
        "their own clock, so a slow discovery pass never lets one lapse",
    )
    ap.add_argument(
        "--probe-workers",
        type=int,
        default=32,
        help="candidates probed at once during discovery",
    )
    ap.add_argument("--probe-timeout", type=float, default=5.0)
    ap.add_argument(
        "--once",
        action="store_true",
        help="register and exit, for a cron or a smoke test",
    )
    args = ap.parse_args()

    # Probing ports is what you do when nothing can tell you. Under
    # --kubernetes something can, so the guess is off unless asked for
    # explicitly -- a cluster that exposes an engine on 9000 is discovered,
    # and one that exposes nothing registers nothing rather than being
    # rummaged through.
    if args.scan_ports is None:
        args.scan_ports = [] if args.kubernetes else [8000, 8001, 8080, 8890]

    # --advertise is optional under --kubernetes: hostNetwork pods are
    # addressed by their own node's IP, resolved from the Nodes API.
    missing = [n for n in ("control", "token") if not getattr(args, n)]
    if missing:
        ap.error("missing required: " + ", ".join("--" + m for m in missing))
    if args.interval >= args.ttl:
        ap.error(
            f"--interval {args.interval} must be well inside --ttl {args.ttl}, "
            "or a single slow beat expires the lease"
        )

    log(f"node={args.node} advertising {args.advertise} to {args.control}")
    if args.once:
        heartbeat(args, discover(args))
        return 0

    # Two clocks, not one. Renewing a lease is one POST per endpoint and must
    # happen every --interval without fail; finding endpoints means probing
    # every candidate the cluster exposes, which takes as long as the slowest
    # of them. On one loop the slow job set the pace of the urgent one, and a
    # lease lapsed whenever a pass outran it -- the provider went degraded
    # between passes while the engine behind it was fine. So discovery runs
    # on its own thread and hands over what it found, and the heartbeat
    # renews whatever was last found.
    known = []
    lock = threading.Lock()
    first_pass = threading.Event()

    def discovery_loop():
        while True:
            try:
                found = discover(args)
            except Exception as e:
                # Keep renewing what was already found: a failed pass says
                # nothing about the endpoints, only about this pass.
                log(f"discovery failed: {e}")
                found = None
            if found is not None:
                with lock:
                    gone = [b for b in known if b not in found]
                    known[:] = found
                for base in gone:
                    # Its lease now lapses, and the control plane degrades it
                    # before it deletes -- the designed way for one to leave.
                    log(f"{base} no longer serves models; letting its lease lapse")
            first_pass.set()
            time.sleep(args.discover_interval)

    threading.Thread(target=discovery_loop, daemon=True).start()
    first_pass.wait()
    while True:
        with lock:
            endpoints = list(known)
        heartbeat(args, endpoints)
        time.sleep(args.interval)


def heartbeat(args, endpoints):
    """Register every endpoint once, which renews the lease of each."""
    if not endpoints:
        # Not an error, and deliberately not a reason to exit: a host whose
        # model is still loading serves nothing for ten minutes or more.
        # The lease lapsing is the correct signal for that, and the control
        # plane degrades before it deletes.
        log("no endpoints serving models on this host yet")
    for base in endpoints:
        try:
            r = register(args, base)
            log(
                f"registered {base} -> provider {r.get('id')} "
                f"{r.get('name')!r} kind={r.get('kind')} "
                f"leased={r.get('leased')}"
            )
        except urllib.error.HTTPError as e:
            log(f"registering {base} failed: {e.code} {e.read()[:200]!r}")
        except Exception as e:
            # Never fatal. The control plane being briefly unreachable is
            # exactly when this process must keep running.
            log(f"registering {base} failed: {e}")


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
