#!/usr/bin/env python3
"""Render the concurrency-sweep results as static SVG for the README.

Static, script-free SVG on purpose: GitHub strips scripts from images, so a
chart that draws itself in JavaScript would render as a blank box on the one
page it exists for. Two variants per chart — GitHub honours
`prefers-color-scheme` through `<picture>`, and a chart legible only on white
is half a chart.

Regenerate after any new measurement:

    python3 bench/compare/make-charts.py

Reads `results/*.jsonl`, writes `../../docs/images/`. Nothing is hand-edited in
between, so a number in a chart and a number in the raw data cannot drift.
"""

import json
import math
import os

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.normpath(os.path.join(HERE, "..", "..", "docs", "images"))

W, H = 560, 300
PAD = {"t": 18, "r": 18, "b": 36, "l": 58}

THEMES = {
    "light": {
        "bg": "none",
        "grid": "#D3DEDE",
        "text": "#7B8D90",
        "a": "#0F8C79",
        "b": "#B4652C",
        "note": "#8A6D1F",
    },
    "dark": {
        "bg": "none",
        "grid": "#24373A",
        "text": "#6C8084",
        "a": "#3FCFB4",
        "b": "#E0954F",
        "note": "#D9B85C",
    },
}

MONO = "ui-monospace,SFMono-Regular,Menlo,Consolas,monospace"
LEVELS = [1, 2, 4, 8, 16, 32, 48, 64]


def load(name):
    """One sweep's results, keyed gateway -> concurrency -> row."""
    rows = {}
    with open(os.path.join(HERE, "results", name)) as f:
        for line in f:
            r = json.loads(line)
            rows.setdefault(r["gw"], {})[r["c"]] = r
    return rows


def x_of(c):
    """Concurrency doubles, so place it on a log2 axis.

    Linear spacing would crush every low-concurrency point against the left
    edge, which is where the interesting difference lives.
    """
    return PAD["l"] + (math.log2(c) / math.log2(64)) * (W - PAD["l"] - PAD["r"])


def svg_chart(
    title, series, theme, log=False, ticks=5, fmt=str, marker=None, ylabel=""
):
    """Render one chart as an SVG string, hand-built so nothing is imported."""
    t = THEMES[theme]
    values = [p[1] for s in series for p in s["points"] if p[1] > 0]
    hi = max(values)
    lo = min(values) if log else 0

    def y_of(v):
        if log:
            frac = (math.log10(max(v, lo)) - math.log10(lo)) / (
                math.log10(hi) - math.log10(lo)
            )
        else:
            frac = v / hi
        return H - PAD["b"] - frac * (H - PAD["t"] - PAD["b"])

    out = [
        (
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
            f'font-family="{MONO}" role="img" aria-label="{title}">'
        )
    ]

    for i in range(ticks + 1):
        v = lo * (hi / lo) ** (i / ticks) if log else hi * i / ticks
        y = y_of(v)
        out.append(
            f'<line x1="{PAD["l"]}" y1="{y:.1f}" x2="{W - PAD["r"]}" y2="{y:.1f}" '
            f'stroke="{t["grid"]}" stroke-width="1"/>'
        )
        out.append(
            f'<text x="{PAD["l"] - 9}" y="{y + 3.5:.1f}" text-anchor="end" font-size="10" '
            f'fill="{t["text"]}">{fmt(v)}</text>'
        )

    for c in LEVELS:
        out.append(
            f'<text x="{x_of(c):.1f}" y="{H - PAD["b"] + 17}" text-anchor="middle" font-size="10" '
            f'fill="{t["text"]}">{c}</text>'
        )
    out.append(
        f'<text x="{(W) / 2:.0f}" y="{H - 4}" text-anchor="middle" font-size="10" '
        f'fill="{t["text"]}">concurrent streams</text>'
    )
    if ylabel:
        out.append(
            f'<text x="12" y="{(H - PAD["b"]) / 2:.0f}" font-size="10" fill="{t["text"]}" '
            f'transform="rotate(-90 12 {(H - PAD["b"]) / 2:.0f})" text-anchor="middle">{ylabel}</text>'
        )

    if marker:
        mx = x_of(marker[0])
        out.append(
            f'<line x1="{mx:.1f}" y1="{PAD["t"]}" x2="{mx:.1f}" y2="{H - PAD["b"]}" '
            f'stroke="{t["note"]}" stroke-width="1" stroke-dasharray="2 4"/>'
        )
        out.append(
            f'<text x="{mx + 6:.1f}" y="{PAD["t"] + 11}" font-size="10" fill="{t["note"]}">{marker[1]}</text>'
        )

    for s in series:
        colour = t[s["colour"]]
        d = " ".join(
            f"{'M' if i == 0 else 'L'}{x_of(x):.1f},{y_of(y):.1f}"
            for i, (x, y) in enumerate(s["points"])
        )
        dash = ' stroke-dasharray="3 3" opacity="0.75"' if s.get("dashed") else ""
        width = 1.4 if s.get("dashed") else 2
        out.append(
            f'<path d="{d}" fill="none" stroke="{colour}" stroke-width="{width}" '
            f'stroke-linejoin="round" stroke-linecap="round"{dash}/>'
        )
        if not s.get("dashed"):
            for x, y in s["points"]:
                out.append(
                    f'<circle cx="{x_of(x):.1f}" cy="{y_of(y):.1f}" r="2.6" fill="{colour}"/>'
                )

    # Legend sits inside the frame so the chart is self-contained wherever it
    # is embedded.
    lx, ly = PAD["l"] + 4, PAD["t"] + 4
    for i, s in enumerate(sr for sr in series if not sr.get("dashed")):
        colour = t[s["colour"]]
        out.append(
            f'<rect x="{lx}" y="{ly + i * 15 - 4}" width="14" height="3" rx="1.5" fill="{colour}"/>'
        )
        out.append(
            f'<text x="{lx + 20}" y="{ly + i * 15}" font-size="10" fill="{t["text"]}">{s["label"]}</text>'
        )

    out.append("</svg>")
    return "\n".join(out)


def write(name, title, series_fn, **kw):
    """One chart per theme, so the README renders in light and dark."""
    os.makedirs(OUT, exist_ok=True)
    for theme in THEMES:
        path = os.path.join(OUT, f"{name}-{theme}.svg")
        with open(path, "w") as f:
            f.write(svg_chart(title, series_fn(), theme, **kw))
        print(f"  wrote {os.path.relpath(path, os.path.join(HERE, '..', '..'))}")


def main():
    """Every chart in bench/compare/charts, from the two sweep files."""
    mock = load("sweep_mock.jsonl")
    real = load("sweep_real.jsonl")

    def pts(rows, gw, key):
        return [(c, rows[gw][c][key]) for c in LEVELS if c in rows[gw]]

    def pair(rows, key):
        return [
            {
                "label": "fastllm-proxy",
                "colour": "a",
                "points": pts(rows, "fastllm", key),
            },
            {"label": "LiteLLM", "colour": "b", "points": pts(rows, "litellm", key)},
        ]

    write(
        "bench-mock-throughput",
        "Requests per second against a mock upstream, by concurrency",
        lambda: pair(mock, "req_s"),
        fmt=lambda v: f"{round(v):,}",
        ylabel="requests/s",
    )
    write(
        "bench-mock-latency",
        "Median time to first token against a mock upstream, log scale",
        lambda: pair(mock, "ttft_p50"),
        log=True,
        ticks=4,
        fmt=lambda v: f"{round(v):,}" if v >= 100 else f"{v:.1f}",
        ylabel="TTFT p50 (ms)",
    )
    write(
        "bench-real-throughput",
        "Aggregate tokens per second against two real vLLM replicas",
        lambda: pair(real, "tok_s"),
        fmt=lambda v: f"{round(v):,}",
        marker=(32, "32 = 2 x 16 GPU slots"),
        ylabel="tokens/s",
    )
    write(
        "bench-real-latency",
        "Time to first token against real vLLM, p50 solid and p99 dotted",
        lambda: (
            pair(real, "ttft_p50")
            + [
                {
                    "label": "",
                    "colour": "a",
                    "dashed": True,
                    "points": pts(real, "fastllm", "ttft_p99"),
                },
                {
                    "label": "",
                    "colour": "b",
                    "dashed": True,
                    "points": pts(real, "litellm", "ttft_p99"),
                },
            ]
        ),
        fmt=lambda v: f"{round(v):,}",
        ylabel="TTFT (ms)",
    )
    write(
        "bench-real-jitter",
        "Standard deviation of the gap between tokens, against real vLLM",
        lambda: pair(real, "gap_sd"),
        ticks=4,
        fmt=lambda v: f"{v:.0f}",
        ylabel="inter-token sd (ms)",
    )


if __name__ == "__main__":
    main()
