// Check the test fixtures against a real control plane.
//
// # Why
//
// Both harnesses are only as truthful as `fixtures.mjs`, and twice now a
// fixture written from the *Rust field name* rather than the wire format hid a
// real bug while the tests stayed green:
//
// - rule conditions nested under `match_condition`, when `RuleView` flattens
//   them — so the screen rendered correctly in the test and every real rule
//   displayed as a catch-all;
// - `model:invoke` with resource `*`, when the API stores `model/*` — so the
//   grants matrix looked right in the test and showed a role with access to
//   every model as having none.
//
// A fixture that disagrees with the API is worse than no fixture: it makes the
// suite confidently wrong. This compares the two and fails on drift.
//
// Skipped unless a control plane is reachable, so `npm test` stays runnable on
// a laptop with nothing running:
//
//   FASTLLM_ADMIN_URL=https://127.0.0.1:14001 \
//   FASTLLM_ADMIN_USER=bootstrap FASTLLM_ADMIN_PASSWORD=... \
//   node test/verify-fixtures.mjs

import { FIXTURES } from "./fixtures.mjs";

const base = process.env.FASTLLM_ADMIN_URL;
if (!base) {
  console.log("verify-fixtures: no FASTLLM_ADMIN_URL — skipped");
  process.exit(0);
}

// A dev control plane commonly has a private cert; this script only ever reads
// admin metadata, never credentials, and refusing to run without a public CA
// would mean it never runs at all.
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

const user = process.env.FASTLLM_ADMIN_USER || "bootstrap";
const password = process.env.FASTLLM_ADMIN_PASSWORD;
if (!password) {
  console.error("verify-fixtures: FASTLLM_ADMIN_PASSWORD is required");
  process.exit(2);
}

const login = await fetch(`${base}/login`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ name: user, password }),
});
if (!login.ok) {
  console.error(`verify-fixtures: login failed (${login.status})`);
  process.exit(2);
}
const cookie = (login.headers.get("set-cookie") || "").split(";")[0];

class Absent extends Error {}

async function get(path) {
  const r = await fetch(`${base}${path}`, { headers: { cookie } });
  // A route this control plane does not have is a deployment older than the
  // code, not a fixture that drifted. Worth saying out loud, not worth
  // failing on — the fixture describes what the code serves.
  if (r.status === 404) throw new Absent(path);
  if (!r.ok) throw new Error(`GET ${path} → ${r.status}`);
  return r.json();
}

/** Every key path in an object, so a renamed or moved field is visible. */
function shape(value, prefix = "", depth = 0) {
  if (depth > 3 || value === null || typeof value !== "object") return [];
  if (Array.isArray(value))
    return value.length ? shape(value[0], `${prefix}[]`, depth + 1) : [];
  return Object.entries(value).flatMap(([k, v]) => {
    const path = prefix ? `${prefix}.${k}` : k;
    const nested = shape(v, path, depth + 1);
    return nested.length ? nested : [path];
  });
}

let failures = 0;
let stale = 0;
function compare(path, live, fixture) {
  // Nothing configured yet: an empty list has no shape to disagree with, and
  // reporting every fixture field as invented would be noise that trains
  // people to ignore this check.
  if (Array.isArray(live) && live.length === 0) {
    console.log(`  — ${path} (no rows on this deployment)`);
    return;
  }
  const liveKeys = new Set(shape(live));
  const fixtureKeys = new Set(shape(fixture));
  // Only fields the fixture claims but the API does not produce are failures.
  // The reverse — a field the UI does not use — is fine and expected.
  const invented = [...fixtureKeys].filter((k) => !liveKeys.has(k));
  if (invented.length) {
    failures++;
    console.log(`  ✗ ${path}`);
    for (const k of invented)
      console.log(`      fixture has ${k}, the API does not`);
    const missing = [...liveKeys]
      .filter((k) => !fixtureKeys.has(k))
      .slice(0, 6);
    if (missing.length)
      console.log(`      (API also returns: ${missing.join(", ")})`);
  } else {
    console.log(`  ✓ ${path}`);
  }
}

console.log(`verify-fixtures: against ${base}`);
for (const path of Object.keys(FIXTURES)) {
  try {
    compare(path, await get(path), FIXTURES[path]);
  } catch (e) {
    if (e instanceof Absent) {
      stale++;
      console.log(
        `  — ${path} (not on this control plane; it predates the route)`,
      );
    } else {
      failures++;
      console.log(`  ✗ ${path} — ${e.message}`);
    }
  }
}

// The two specific values that have already gone wrong, asserted by value
// rather than by shape — a namespaced resource and a flattened condition are
// both structurally invisible.
if (stale) {
  console.log(
    `\n  ${stale} route(s) absent — this control plane is older than the code. ` +
      "Re-run after deploying to check those fixtures.",
  );
}

const roles = await get("/admin/roles");
const wildcard = roles
  .flatMap((r) => r.permissions)
  .find((p) => p.verb === "model:invoke" && p.resource.includes("*"));
if (wildcard && wildcard.resource !== "model/*") {
  failures++;
  console.log(
    `  ✗ blanket model grant is ${wildcard.resource}, fixtures assume model/*`,
  );
} else if (wildcard) {
  console.log("  ✓ blanket model grant is model/*");
}

const vms = await get("/admin/frontend-models");
const rule = vms.flatMap((v) => v.rules)[0];
if (rule) {
  if ("match_condition" in rule) {
    failures++;
    console.log(
      "  ✗ rule conditions are nested; fixtures and the UI assume flattened",
    );
  } else {
    console.log("  ✓ rule conditions are flattened onto the rule");
  }
} else {
  console.log("  — no rule to check conditions against");
}

process.exit(failures ? 1 : 0);
