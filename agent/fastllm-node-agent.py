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
import time
import urllib.error
import urllib.request

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
    """True when something at `base` answers the one call that matters."""
    try:
        req = urllib.request.Request(base.rstrip("/") + MODELS_PATH)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            body = json.load(r)
        return isinstance(body.get("data"), list)
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
        for port in spec.get("ports") or []:
            if override:
                found.append(
                    f"http://{override}/v1" if "://" not in override else override
                )
                continue
            if kind == "LoadBalancer":
                for ing in ((svc.get("status") or {}).get("loadBalancer") or {}).get(
                    "ingress", []
                ):
                    addr = ing.get("ip") or ing.get("hostname")
                    if addr and port.get("port"):
                        found.append(f"http://{addr}:{port['port']}/v1")
            elif kind == "NodePort" and port.get("nodePort") and args.advertise:
                found.append(f"http://{args.advertise}:{port['nodePort']}/v1")

    # A hostPort or host networking bypasses Services entirely and is a normal
    # way to expose a single-node engine, so both would be invisible to a
    # Service-only scan.
    try:
        pods = kube_get("/api/v1/pods", args.probe_timeout).get("items", [])
    except Exception as e:
        log(f"could not list pods: {e}")
        pods = []
    for pod in pods:
        spec = pod.get("spec") or {}
        host_net = bool(spec.get("hostNetwork"))
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
            for hp in ports:
                if args.advertise:
                    found.append(f"http://{args.advertise}:{hp}/v1")

    # Two Services can front the same endpoint; register it once.
    return list(dict.fromkeys(found))


def discover(args):
    """Endpoints on this host that serve models.

    Sources compose and are all optional, which is what makes "in Docker or
    not" fall out rather than being a mode. The port probe alone covers a bare
    process started by hand or by a launcher, with no container runtime
    present at all.
    """
    found = []
    for base in args.api_base:
        if serves_models(base, args.probe_timeout):
            found.append(base)
        else:
            log(f"configured endpoint {base} did not answer {MODELS_PATH}")

    if args.kubernetes:
        for base in kube_candidates(args):
            if base not in found and serves_models(base, args.probe_timeout):
                found.append(base)

    for port in args.scan_ports:
        # The advertised host, never a loopback or a container address: this
        # is the address the *proxies* will dial. An agent that discovers a
        # container on 172.17.0.2 and registers that hands the proxies an
        # address they cannot reach.
        base = f"http://{args.advertise}:{port}/v1"
        if base not in found and serves_models(base, args.probe_timeout):
            found.append(base)
    return found


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


def provider_name(args, api_base):
    """What to call this endpoint in FastLLM.

    The name lives here rather than on the control plane, which only ever sees
    an address. `dgx-spark-8000` beats `192.168.10.246:8000` on a screen, and
    an operator who renames the host's agent renames what they are looking at.

    The port is always appended, never only when a host happens to serve more
    than one endpoint: a name that changes shape as a second model is started
    would rename the first one behind the operator's back.
    """
    base = args.provider_name or args.node
    tail = api_base.split("://", 1)[-1].split("/")[0]
    port = tail.rsplit(":", 1)[-1] if ":" in tail else ""
    return f"{base}-{port}" if port else base


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

    missing = [n for n in ("control", "token", "advertise") if not getattr(args, n)]
    if missing:
        ap.error("missing required: " + ", ".join("--" + m for m in missing))
    if args.interval >= args.ttl:
        ap.error(
            f"--interval {args.interval} must be well inside --ttl {args.ttl}, "
            "or a single slow beat expires the lease"
        )

    log(f"node={args.node} advertising {args.advertise} to {args.control}")
    while True:
        endpoints = discover(args)
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
        if args.once:
            return 0
        time.sleep(args.interval)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
