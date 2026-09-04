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

    for port in args.scan_ports:
        # The advertised host, never a loopback or a container address: this
        # is the address the *proxies* will dial. An agent that discovers a
        # container on 172.17.0.2 and registers that hands the proxies an
        # address they cannot reach.
        base = f"http://{args.advertise}:{port}/v1"
        if base not in found and serves_models(base, args.probe_timeout):
            found.append(base)
    return found


def register(args, api_base):
    body = json.dumps(
        {
            "api_base": api_base,
            "node": args.node,
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
    with urllib.request.urlopen(req, timeout=args.probe_timeout) as r:
        return json.load(r)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--control", default=os.environ.get("FASTLLM_CONTROL_URL"),
                    help="control plane admin base URL")
    ap.add_argument("--token", default=os.environ.get("FASTLLM_AGENT_TOKEN"),
                    help="API key for this node's principal")
    ap.add_argument("--node", default=os.environ.get("FASTLLM_NODE", socket.gethostname()),
                    help="name for this host, scoping what it may register")
    ap.add_argument("--advertise", default=os.environ.get("FASTLLM_ADVERTISE"),
                    help="address the proxies should dial. Configured, never "
                         "inferred: a discovered container address is one the "
                         "proxies cannot reach")
    ap.add_argument("--api-base", action="append", default=[],
                    help="an endpoint to register outright; repeatable")
    ap.add_argument("--scan-ports", type=int, nargs="*", default=[8000, 8001, 8080, 8890],
                    help="ports on --advertise to probe")
    ap.add_argument("--engine", default=os.environ.get("FASTLLM_ENGINE"),
                    help="hint only; nothing depends on it")
    ap.add_argument("--ttl", type=int, default=90,
                    help="lease length in seconds")
    ap.add_argument("--interval", type=int, default=30,
                    help="how often to re-register. Well inside --ttl, so one "
                         "missed beat is not an expiry")
    ap.add_argument("--probe-timeout", type=float, default=5.0)
    ap.add_argument("--once", action="store_true",
                    help="register and exit, for a cron or a smoke test")
    args = ap.parse_args()

    missing = [n for n in ("control", "token", "advertise") if not getattr(args, n)]
    if missing:
        ap.error("missing required: " + ", ".join("--" + m for m in missing))
    if args.interval >= args.ttl:
        ap.error(f"--interval {args.interval} must be well inside --ttl {args.ttl}, "
                 "or a single slow beat expires the lease")

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
                log(f"registered {base} -> provider {r.get('id')} "
                    f"kind={r.get('kind')} leased={r.get('leased')}")
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
