#!/usr/bin/env python3
"""Download the labelled prompt sets the routing-classifier benchmark evaluates on.

Two datasets, for the two questions the benchmark asks:

  HuggingFaceH4/no_robots  — ~9.5k human-written prompts, each carrying a real
                             `category` label (Coding, Open QA, Chat, ...). This
                             is the domain-routing question, with labels written
                             by people rather than by whoever wrote the
                             benchmark, which is the whole point of using it.

  openai/gsm8k             — grade-school maths word problems, i.e. prompts that
                             genuinely need step-by-step reasoning. Paired
                             against no_robots' factual "Open QA" prompts, this
                             is the *difficulty* question: can a static
                             embedding tell "needs the expensive model" from
                             "needs a lookup"? Nobody labels that, so it has to
                             be constructed.

Stdlib only, and cached: rerunning is free once the JSON is on disk. Writes to
`bench/data/`, which is git-ignored — this is measurement input, not source.

    python3 bench/fetch-prompts.py
"""

import json
import os
import sys
import time
import urllib.parse
import urllib.request

ROWS = "https://datasets-server.huggingface.co/rows"
PAGE = 100
DATA_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")


def fetch_rows(dataset, config, split, limit, start=0):
    """Page through the datasets-server API. 100 rows per request is its cap."""
    out = []
    offset = start
    while offset < limit:
        params = urllib.parse.urlencode(
            {
                "dataset": dataset,
                "config": config,
                "split": split,
                "offset": offset,
                "length": min(PAGE, limit - offset),
            }
        )
        url = f"{ROWS}?{params}"
        payload = None
        for attempt in range(8):
            try:
                with urllib.request.urlopen(url, timeout=90) as r:
                    payload = json.load(r)
                break
            except urllib.error.HTTPError as e:
                # The datasets-server rate-limits an unauthenticated caller
                # after a few thousand rows. Backing off exponentially and
                # honouring Retry-After is the difference between finishing and
                # silently returning a truncated set — which would quietly
                # change what the benchmark measures.
                wait = int(e.headers.get("Retry-After") or 0) or min(60, 2**attempt * 3)
                if attempt == 7:
                    print(f"\n  giving up at offset {offset}: {e}", file=sys.stderr)
                    return out
                print(
                    f"\n  {e.code} at offset {offset}, waiting {wait}s", file=sys.stderr
                )
                time.sleep(wait)
            except Exception as e:  # noqa: BLE001 - a retry loop wants them all
                if attempt == 7:
                    print(f"\n  giving up at offset {offset}: {e}", file=sys.stderr)
                    return out
                time.sleep(min(30, 2**attempt))
        if payload is None:
            return out
        rows = payload.get("rows", [])
        if not rows:
            break
        out.extend(r["row"] for r in rows)
        offset += len(rows)
        # Gentle by default; the whole fetch is a one-off and being throttled
        # halfway costs far more than the pause does.
        time.sleep(0.35)
        print(f"\r  {dataset}: {len(out)} rows", end="", file=sys.stderr)
    print(file=sys.stderr)
    return out


def load(name):
    """Whatever a previous (possibly throttled) run already saved."""
    path = os.path.join(DATA_DIR, name)
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return json.load(f)


def write(name, records):
    """Save the whole set; partial progress survives a throttled run."""
    os.makedirs(DATA_DIR, exist_ok=True)
    path = os.path.join(DATA_DIR, name)
    with open(path, "w") as f:
        json.dump(records, f)
    print(f"  wrote {len(records)} to {path}")


def main():
    """Top up the prompt sets to their targets, resuming where a run stopped."""
    target = 9500
    existing = load("no_robots.json")
    if len(existing) < target:
        rows = fetch_rows(
            "HuggingFaceH4/no_robots", "default", "train", target, start=len(existing)
        )
        # Only the prompt and its label; `messages` holds the answer, which the
        # router never sees and must not be fitted to.
        existing.extend(
            {"prompt": r["prompt"], "category": r["category"]}
            for r in rows
            if r.get("prompt") and r.get("category")
        )
        write("no_robots.json", existing)
    else:
        print(f"  no_robots.json complete ({len(existing)} rows)")

    # The architecture-versus-coding question, as a natural experiment rather
    # than invented seeds: two StackExchange sites whose communities separated
    # the traffic themselves. softwareengineering.SE is where design and
    # architecture questions live; codereview.SE is concrete code. Both are
    # written by programmers about programs, which is exactly what makes the
    # separation hard and therefore worth measuring.
    # Beyond the architecture question, a spread of *domains* an enterprise
    # would plausibly route on. Each is its own StackExchange community, so the
    # boundaries were drawn by the people asking rather than by us — the same
    # natural-experiment property that made the architecture test trustworthy.
    for config, name in [
        ("softwareengineering", "se_architecture.json"),
        ("codereview", "se_codereview.json"),
        ("devops", "se_devops.json"),
        ("security", "se_security.json"),
        ("dba", "se_dba.json"),
        ("datascience", "se_datascience.json"),
        ("stats", "se_stats.json"),
        ("law", "se_law.json"),
        ("money", "se_money.json"),
        ("ux", "se_ux.json"),
        ("writers", "se_writers.json"),
    ]:
        target = 1200
        existing = load(name)
        if len(existing) < target:
            rows = fetch_rows(
                "flax-sentence-embeddings/stackexchange_title_best_voted_answer_jsonl",
                config,
                "train",
                target,
                start=len(existing),
            )
            existing.extend(
                {"prompt": r["title_body"], "category": config}
                for r in rows
                if r.get("title_body")
            )
            write(name, existing)
        else:
            print(f"  {name} complete ({len(existing)} rows)")

    target = 1000
    existing = load("gsm8k.json")
    if len(existing) < target:
        rows = fetch_rows("openai/gsm8k", "main", "train", target, start=len(existing))
        existing.extend(
            {"prompt": r["question"], "category": "Math"}
            for r in rows
            if r.get("question")
        )
        write("gsm8k.json", existing)
    else:
        print(f"  gsm8k.json complete ({len(existing)} rows)")


if __name__ == "__main__":
    main()
