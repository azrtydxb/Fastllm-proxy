#!/usr/bin/env python3
"""Generate the endpoint reference inside .claude/skills/*/SKILL.md.

Three sources, because no single one is complete:

  openapi.json      paths, methods, summaries -- authoritative for *coverage*,
                    but it carries no schemas (components holds only
                    securitySchemes), so it cannot describe a request body.
  src/control/api.rs the axum route table, handler signatures, and the
                    #[derive(Deserialize)] structs -- the only place the
                    request fields exist.
  SKILL.md itself   everything outside the generated markers is hand-written
                    and preserved: the traps, which no spec contains.

Run: python3 scripts/gen-claude-skills.py [--check]
`--check` exits non-zero when a skill is stale, so CI catches drift rather
than letting the skills quietly stop matching the API.
"""
from __future__ import annotations
import json, re, sys, pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
API_RS = ROOT / "src/control/api.rs"
OPENAPI = ROOT / "openapi.json"
SKILLS = ROOT / ".claude/skills"

BEGIN = "<!-- BEGIN GENERATED: endpoints -->"
END = "<!-- END GENERATED: endpoints -->"

# Which paths belong to which skill. Every path in openapi.json must match
# exactly one skill, or --check fails: that is what makes "all functions are
# exposed" a property the build enforces rather than a claim in a README.
DOMAINS: dict[str, list[str]] = {
    "fastllm-auth": [r"^/login$", r"^/logout$", r"^/admin/sessions/", r"/password$"],
    "fastllm-principals": [r"^/admin/principals", r"^/admin/keys"],
    "fastllm-roles": [r"^/admin/roles"],
    "fastllm-models": [r"^/admin/provider-models", r"^/admin/providers", r"^/admin/provider-catalogue", r"^/admin/backends/", r"^/admin/fallback-model$"],
    "fastllm-routing": [r"^/admin/frontend-model", r"^/admin/rules", r"^/admin/rule-targets/",
                        r"^/admin/routing/"],
    "fastllm-classifier": [r"^/admin/prompt-classes"],
    "fastllm-limits-budgets": [r"^/admin/limits$", r"^/admin/budgets$", r"^/limits/reconcile$",
                               r"^/admin/prices/"],
    "fastllm-observability": [r"^/admin/usage$", r"^/admin/timeseries$", r"^/admin/audit$",
                              r"^/metrics$", r"^/admin/health$", r"^/admin/fleet$"],
    "fastllm-deployment": [r"^/admin/config$", r"^/admin/deployment$", r"^/admin/snapshot/",
                           r"^/snapshot$", r"^/health-report$", r"^/usage$", r"^/health$",
                           r"^/healthz$", r"^/docs$", r"^/openapi.json$"],
    "fastllm-gateway": [r"^/v1/(chat|completions|embeddings|rerank|responses|moderations|audio|images|models|score)"],
    "fastllm-mcp": [r"^/admin/mcp-servers", r"^/v1/mcp/"],
    "fastllm-agents": [r"^/admin/a2a-agents", r"^/v1/agents"],
}

METHODS = ("get", "post", "put", "patch", "delete")


def parse_routes(src: str) -> dict[tuple[str, str], str]:
    """(path, METHOD) -> handler fn name, from the axum router."""
    out: dict[tuple[str, str], str] = {}
    # Take a window after each .route("path", ... rather than trying to match
    # balanced parens: a non-greedy `(.+?)\)` stops inside `get(handler)` and
    # silently drops the `.post(handler)` half of a chained route.
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', src):
        path = m.group(1)
        chain = src[m.end(): m.end() + 300]
        chain = chain.split(".route(")[0]
        for verb, fn in re.findall(r"\b(get|post|put|patch|delete)\(([a-z_0-9]+)\)", chain):
            out[(path, verb.upper())] = fn
    return out


def parse_handler_bodies(src: str) -> dict[str, str]:
    """handler fn name -> request struct name, from `Json(body): Json<T>`."""
    out: dict[str, str] = {}
    for m in re.finditer(r"async fn (\w+)\(((?:[^()]|\([^()]*\))*)\)", src):
        fn, args = m.group(1), m.group(2)
        j = re.search(r"Json\(\s*\w+\s*\)\s*:\s*Json<([\w:]+)>", args)
        if j:
            out[fn] = j.group(1).split("::")[-1]
    return out


def parse_structs(src: str) -> dict[str, list[tuple[str, str, bool]]]:
    """struct name -> [(field, type, required)] for Deserialize request types."""
    out: dict[str, list[tuple[str, str, bool]]] = {}
    for m in re.finditer(r"((?:#\[[^\]]*\]\s*)+)struct\s+(\w+)\s*\{(.*?)\n\}", src, re.S):
        attrs, name, body = m.group(1), m.group(2), m.group(3)
        if "Deserialize" not in attrs:
            continue
        fields: list[tuple[str, str, bool]] = []
        for fm in re.finditer(r"((?:\s*#\[[^\]]*\]\s*)*)\s*(?:pub\s+)?(\w+)\s*:\s*([^,\n]+),", body):
            fattrs, fname, ftype = fm.group(1), fm.group(2), fm.group(3).strip()
            if fname.startswith("_"):
                continue
            optional = "default" in fattrs or ftype.startswith("Option<")
            fields.append((fname, ftype, not optional))
        if fields:
            out[name] = fields
    return out


def build_table(paths: dict, routes, handlers, structs, patterns: list[str]) -> tuple[str, list[str]]:
    rows, matched = [], []
    for path in sorted(paths):
        if not any(re.search(p, path) for p in patterns):
            continue
        matched.append(path)
        for verb in METHODS:
            if verb not in paths[path]:
                continue
            V = verb.upper()
            summary = (paths[path][verb].get("summary") or "").strip()
            fn = routes.get((path, V))
            body = "—"
            if fn and fn in handlers:
                st = handlers[fn]
                fs = structs.get(st)
                if fs:
                    body = ", ".join(f"`{n}`" + ("" if req else "*") for n, _t, req in fs)
            rows.append(f"| `{V}` | `{path}` | {summary} | {body} |")
    table = ["| Method | Path | Summary | Body fields |", "|---|---|---|---|"] + rows
    return "\n".join(table), matched


def main() -> int:
    check = "--check" in sys.argv
    src = API_RS.read_text()
    paths = json.loads(OPENAPI.read_text())["paths"]
    routes, handlers, structs = parse_routes(src), parse_handler_bodies(src), parse_structs(src)

    covered: set[str] = set()
    stale: list[str] = []
    for skill, patterns in DOMAINS.items():
        table, matched = build_table(paths, routes, handlers, structs, patterns)
        covered.update(matched)
        f = SKILLS / skill / "SKILL.md"
        if not f.exists():
            print(f"  (no SKILL.md yet: {skill}) — {len(matched)} paths would be generated")
            continue
        text = f.read_text()
        if BEGIN not in text or END not in text:
            print(f"  !! {skill}: missing generated markers", file=sys.stderr)
            stale.append(skill)
            continue
        new = re.sub(re.escape(BEGIN) + r".*?" + re.escape(END),
                     f"{BEGIN}\n\n{table}\n\n*\\* optional field*\n\n{END}", text, flags=re.S)
        if new != text:
            stale.append(skill)
            if not check:
                f.write_text(new)
                print(f"  updated {skill} ({len(matched)} paths)")
        else:
            print(f"  up to date {skill} ({len(matched)} paths)")

    missing = sorted(set(paths) - covered)
    if missing:
        print(f"\n!! {len(missing)} endpoint(s) belong to no skill:", file=sys.stderr)
        for p in missing:
            print(f"     {p}", file=sys.stderr)
        return 1
    print(f"\nall {len(paths)} endpoints are covered by a skill")
    if check and stale:
        print(f"!! stale skills: {', '.join(stale)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
