#!/usr/bin/env python3
"""Minimal fastllm-proxy webhook receiver that verifies the signature.

Run:
    FASTLLM_WEBHOOK_SECRET=... python3 examples/webhook-receiver.py
    fastllm-proxy ... --webhook-url http://localhost:8099/hook \
                      --webhook-secret "$FASTLLM_WEBHOOK_SECRET"

The signature check is the point of this file. A webhook endpoint is reachable
by anyone who learns its address, so a receiver that *acts* on notifications --
pages someone, restarts something -- must not act on an unauthenticated POST.
`compare_digest` rather than `==` because a byte-at-a-time comparison leaks
where the first mismatch was, and this one is checked on attacker-supplied
input by definition.
"""

import hashlib
import hmac
import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer

SECRET = os.environ.get("FASTLLM_WEBHOOK_SECRET", "").encode()


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))

        if SECRET:
            expected = "sha256=" + hmac.new(SECRET, body, hashlib.sha256).hexdigest()
            got = self.headers.get("x-fastllm-signature", "")
            if not hmac.compare_digest(expected, got):
                self.send_response(401)
                self.end_headers()
                print("rejected: bad signature")
                return

        event = json.loads(body)
        kind = event.get("event")

        # One line per event rather than a dump, because the interesting part
        # of an alert is what changed and where.
        if kind in ("backend_down", "backend_recovered"):
            print(
                f"[{event['at']}] {kind}: {event['model']} @ {event['api_base']} "
                f"(reported by {event['replica']})"
            )
        elif kind == "snapshot_rebuild_failed":
            print(
                f"[{event['at']}] snapshot rebuild failed "
                f"({event['consecutive']} in a row): {event['error']}"
            )
        else:
            print(f"[{event.get('at')}] {kind}: {event}")

        self.send_response(204)
        self.end_headers()

    def log_message(self, *args):
        pass  # the handler above already prints what matters


if __name__ == "__main__":
    if not SECRET:
        print("warning: FASTLLM_WEBHOOK_SECRET unset — signatures will not be checked")
    HTTPServer(("0.0.0.0", 8099), Handler).serve_forever()
