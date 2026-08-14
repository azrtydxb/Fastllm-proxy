// Click everything, and check what each control actually sends.
//
// # Why this is separate from render.mjs
//
// `render.mjs` answers "does the screen come up". It clicks two buttons out of
// roughly sixty, which meant every form and every destructive control shipped
// unexercised — and the worst bug in this UI so far was exactly that shape: the
// rule builder posted `{position, match_condition: {...}}` when the handler
// flattens the conditions, so serde discarded them, answered 201, and every
// rule created through the UI matched every request. Nothing rendered wrong.
// Only the request body was wrong, and no test looked at a request body.
//
// So this file does two passes:
//
// 1. **Coverage.** Click every button on every screen and fail on a thrown
//    render or a console error. Nothing real happens — `fetch` is stubbed — so
//    clicking "delete" is safe, and a control that throws is found without
//    anybody having to predict which one would.
//
// 2. **Contract.** Drive the mutations that matter and assert the exact
//    method, path and body against what the Rust handler deserialises. This is
//    the pass that would have caught the flatten bug.
//
// The fixtures are shared with render.mjs so there is one description of what
// the API returns, and it is the wire format rather than the struct shape.

import { JSDOM } from "jsdom";
import { FIXTURES, USAGE, fixtureFor } from "./fixtures.mjs";

const dom = new JSDOM('<!doctype html><html><body><div id="root"></div></body></html>', {
  url: "http://localhost/",
  pretendToBeVisual: true,
});

const problems = [];
dom.window.console.error = (...args) => problems.push(args.map(String).join(" "));
// Every destructive control asks first; a harness that answered "no" would
// exercise none of them.
dom.window.confirm = () => true;
dom.window.alert = () => {};

globalThis.window = dom.window;
globalThis.document = dom.window.document;
Object.defineProperty(globalThis, "navigator", {
  value: dom.window.navigator,
  configurable: true,
});
globalThis.HTMLElement = dom.window.HTMLElement;
globalThis.Node = dom.window.Node;
globalThis.getComputedStyle = dom.window.getComputedStyle;
globalThis.requestAnimationFrame = (cb) => setTimeout(cb, 0);
globalThis.cancelAnimationFrame = clearTimeout;
globalThis.console.error = dom.window.console.error;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;

/** Every request the UI made, in order. */
const sent = [];
globalThis.fetch = async (path, init = {}) => {
  const method = init.method || "GET";
  let body = null;
  if (init.body) {
    try {
      body = JSON.parse(init.body);
    } catch {
      body = init.body;
    }
  }
  sent.push({ method, path, body });
  const fixture = fixtureFor(path);
  // A created key must come back with plaintext once, or the Keys screen has
  // nothing to show and the reveal panel never renders.
  const answer =
    method === "POST" && path === "/admin/keys"
      ? { id: 99, key: "sk-plaintext-shown-once" }
      : method === "POST" && path.endsWith("/backends")
        ? { id: 77 }
        : method === "POST" && path.includes("/rules")
          ? { id: 88 }
          : method === "POST" && path === "/admin/prices/sync"
            ? { updated: 1, already_priced: 2, unmatched: 0, dry_run: true, changes: [] }
            : method === "POST" && path === "/admin/routing/dry-run"
              ? { candidates: ["claude-sonnet", "local-qwen"], matched_rule: 0, virtual_model: true }
              : method === "POST" && path === "/admin/prompt-classes/evaluate"
                ? { classes: [], note: "no classifier" }
                : fixture;
  return {
    ok: true,
    status: 200,
    headers: { get: () => "application/json" },
    json: async () => (answer === null ? {} : answer),
  };
};

const { createServer } = await import("vite");
const vite = await createServer({
  configFile: false,
  root: new URL("..", import.meta.url).pathname,
  server: { middlewareMode: true, hmr: false },
  appType: "custom",
  plugins: [(await import("@vitejs/plugin-react")).default()],
  logLevel: "warn",
});
const React = (await import("react")).default;
const { createRoot } = await import("react-dom/client");
const { act } = await import("react");
const { App } = await vite.ssrLoadModule("/src/App.jsx");

const root = createRoot(document.getElementById("root"));

let failures = 0;
function check(name, condition, detail) {
  if (condition) {
    console.log(`    ✓ ${name}`);
  } else {
    failures++;
    console.log(`    ✗ ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

async function settle() {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 25));
  });
}

async function goto(screen) {
  await act(async () => {
    dom.window.location.hash = `#/${screen}`;
    dom.window.dispatchEvent(new dom.window.Event("hashchange"));
  });
  await settle();
}

async function click(el, what) {
  if (!el) {
    failures++;
    console.log(`    ✗ could not find ${what || "the control to click"}`);
    return false;
  }
  await act(async () => {
    el.dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
  });
  await settle();
  return true;
}

/** Set a controlled input's value the way React's onChange expects. */
async function fill(el, value, what) {
  if (!el) {
    failures++;
    console.log(`    ✗ could not find ${what || "the field to fill"}`);
    return false;
  }
  const proto =
    el.tagName === "SELECT"
      ? dom.window.HTMLSelectElement.prototype
      : el.tagName === "TEXTAREA"
        ? dom.window.HTMLTextAreaElement.prototype
        : dom.window.HTMLInputElement.prototype;
  const setter = Object.getOwnPropertyDescriptor(proto, "value").set;
  await act(async () => {
    setter.call(el, value);
    el.dispatchEvent(new dom.window.Event("input", { bubbles: true }));
    el.dispatchEvent(new dom.window.Event("change", { bubbles: true }));
  });
  await settle();
  return true;
}

const $ = (sel) => [...document.querySelectorAll(sel)];
const byText = (text, sel = "button") =>
  $(sel).find((e) => e.textContent.trim().toLowerCase() === text.toLowerCase());
const containing = (text, sel = "button") =>
  $(sel).find((e) => e.textContent.trim().toLowerCase().includes(text.toLowerCase()));

function lastCall(method, pathIncludes) {
  return [...sent]
    .reverse()
    .find((r) => r.method === method && r.path.includes(pathIncludes));
}

await act(async () => {
  root.render(React.createElement(App));
});
await settle();

const SCREENS = [
  "overview",
  "metrics",
  "usage",
  "providers",
  "models",
  "routing",
  "classes",
  "mcp",
  "agents",
  "keys",
  "rbac",
  "limits",
  "audit",
  "deployment",
  "fleet",
  "settings",
];

// --- pass 1: every button on every screen ----------------------------------

console.log("\nPass 1 — clicking every control");
let clicked = 0;
for (const screen of SCREENS) {
  await goto(screen);
  problems.length = 0;
  // Re-queried each iteration: a click can re-render and replace the DOM, so a
  // list captured up front goes stale and clicking a detached node tests
  // nothing.
  const seen = new Set();
  if (process.env.DEBUG_BUTTONS === screen) {
    console.log(
      "      buttons:",
      $("button").map((b) => JSON.stringify(b.textContent.trim())).join(" "),
    );
  }
  let guard = 0;
  let threw = null;
  while (guard++ < 60) {
    // "exit" logs out, and every screen after it would be the login form —
    // which is how the first run of this harness reported "1 control" on
    // twelve screens and passed. It gets its own check below instead.
    const next = $("button").find(
      (b) => b.textContent.trim() !== "exit" && !seen.has(b.textContent.trim() + b.className),
    );
    if (!next) break;
    seen.add(next.textContent.trim() + next.className);
    try {
      await click(next);
      clicked++;
    } catch (e) {
      threw = `${next.textContent.trim() || "(unlabelled)"}: ${e.message}`;
      break;
    }
    // A click may have navigated away; come back so the rest of this screen's
    // controls are still reachable.
    if (!dom.window.location.hash.includes(screen)) await goto(screen);
  }
  const real = problems.filter((p) => !/not wrapped in act|Not implemented/i.test(p));
  check(
    `${screen}: ${seen.size} controls`,
    !threw && real.length === 0,
    threw || real[0]?.split("\n")[0],
  );
}
console.log(`  ${clicked} controls clicked`);

// --- pass 2: the request each mutation actually sends -----------------------

console.log("\nPass 2 — what the controls send");

// Rules: the conditions are flattened on the wire. This is the assertion the
// flatten bug would have failed.
await goto("routing");
await click(byText("+ Add rule"));
{
  const labelled = (label) => {
    const field = $("label").find((l) => l.textContent.includes(label));
    return field?.querySelector("input, select");
  };
  await fill(labelled("PROMPT CLASS"), "coding");
  await fill(labelled("ROLES (any of)"), "engineering");
  await fill(labelled("STREAM"), "true");
  await fill(labelled("PROMPT TOKENS ≥"), "256");
  await click(byText("Create rule"));
  const call = lastCall("POST", "/rules");
  check("rule POST reaches /admin/virtual-models/{id}/rules", !!call, "no request was sent");
  if (call) {
    check(
      "conditions are flattened, not nested",
      call.body.class === "coding" && call.body.match_condition === undefined,
      `body was ${JSON.stringify(call.body)}`,
    );
    check(
      "roles is an array and numbers are numbers",
      Array.isArray(call.body.roles) &&
        call.body.roles[0] === "engineering" &&
        call.body.min_prompt_tokens === 256 &&
        call.body.stream === true,
      `body was ${JSON.stringify(call.body)}`,
    );
    check("position is sent", typeof call.body.position === "number");
  }
}

// Dry-run: the panel that answers "why did it route there".
await goto("routing");
await click(byText("Dry-run a request"));
await click(byText("Evaluate"));
{
  const call = lastCall("POST", "/admin/routing/dry-run");
  check("dry-run posts the virtual model's name", call?.body?.model === "gpt-router");
  check("dry-run sends streaming as a boolean", typeof call?.body?.streaming === "boolean");
  check(
    "dry-run result renders the matched rule",
    document.getElementById("root").textContent.includes("RULE 0"),
  );
}

// Models: the PATCH semantics that silently cleared a price when mistyped.
await goto("models");
{
  await click(containing("edit"));
  const priceInput = $("label")
    .find((l) => l.textContent.includes("INPUT $ / MTOK"))
    ?.querySelector("input");
  await fill(priceInput, "3.5");
  await click(byText("Save"));
  const call = lastCall("PATCH", "/admin/models/");
  check("price edit sends PATCH", !!call, "no PATCH was sent");
  check(
    "dollars are converted to micro-units",
    call?.body?.input_price_per_mtok === 3500000,
    `sent ${JSON.stringify(call?.body?.input_price_per_mtok)}`,
  );

  // The bug: Number("3,5") is NaN, JSON.stringify makes it null, and null
  // means *clear* — a typo turned a priced model unpriced.
  sent.length = 0;
  await goto("models");
  await click(containing("edit"));
  const again = $("label")
    .find((l) => l.textContent.includes("INPUT $ / MTOK"))
    ?.querySelector("input");
  await fill(again, "3,5");
  await click(byText("Save"));
  check(
    "an unparseable price sends nothing at all",
    !lastCall("PATCH", "/admin/models/"),
    "a PATCH was sent for input that cannot be read",
  );
  check(
    "and says why",
    document.getElementById("root").textContent.includes("is not a number"),
  );

  // Context length: empty must clear it to null rather than send 0. The
  // handler refuses a non-positive length, so sending 0 would surface as a
  // 400 an operator cannot act on -- and reading empty as "unlimited" would
  // undo the routing rule this field exists for.
  sent.length = 0;
  await goto("models");
  await click(containing("edit"));
  const ctx = $("label")
    .find((l) => l.textContent.includes("CONTEXT LENGTH"))
    ?.querySelector("input");
  check("the edit form offers a context length", !!ctx, "no CONTEXT LENGTH field");
  if (ctx) {
    await fill(ctx, "");
    await click(byText("Save"));
    const cleared = lastCall("PATCH", "/admin/models/");
    check(
      "clearing the context length sends null, not 0",
      cleared?.body?.context_length === null,
      `body was ${JSON.stringify(cleared?.body)}`,
    );

    sent.length = 0;
    await goto("models");
    await click(containing("edit"));
    const ctx2 = $("label")
      .find((l) => l.textContent.includes("CONTEXT LENGTH"))
      ?.querySelector("input");
    await fill(ctx2, "262144");
    await click(byText("Save"));
    const set = lastCall("PATCH", "/admin/models/");
    check(
      "a context length is sent as a number",
      set?.body?.context_length === 262144,
      `body was ${JSON.stringify(set?.body)}`,
    );
  }
}

// Backends: the add-backend row.
await goto("models");
{
  const base = $("input").find((i) => i.placeholder === "api_base");
  const upstream = $("input").find((i) => i.placeholder === "upstream_model");
  await fill(base, "http://10.0.0.9:8000/v1");
  await fill(upstream, "some-model");
  await click(byText("Add backend"));
  const call = lastCall("POST", "/backends");
  check("backend POST is sent", !!call);
  check(
    "with the fields the handler deserialises",
    call?.body?.api_base === "http://10.0.0.9:8000/v1" &&
      call?.body?.upstream_model === "some-model",
    `body was ${JSON.stringify(call?.body)}`,
  );
}

// Keys: create, and the once-only reveal.
await goto("keys");
{
  const name = $("input").find((i) => i.placeholder === "ci-runner");
  await fill(name, "handover");
  const principal = $("select").find((s) =>
    [...s.options].some((o) => o.textContent.includes("batch-etl")),
  );
  await fill(principal, [...principal.options].find((o) => o.textContent.includes("batch-etl")).value);
  await click(byText("Create key"));
  const call = lastCall("POST", "/admin/keys");
  check("key POST sends a numeric principal_id", typeof call?.body?.principal_id === "number");
  check("key POST sends an ISO expiry", typeof call?.body?.expires_at === "string");
  check(
    "the plaintext is shown once",
    document.getElementById("root").textContent.includes("sk-plaintext-shown-once"),
  );
}

// RBAC: the permission matrix writes, and the model-grant wildcard.
await goto("rbac");
{
  await click(byText("Permission matrix"));
  const cell = $("button").find((b) => b.textContent.trim() === "·");
  if (cell) {
    await click(cell);
    const call = lastCall("POST", "/permissions");
    check("granting a permission POSTs verb and resource", !!call?.body?.verb);
    check("an admin verb is granted unscoped", call?.body?.resource === "*");
  } else {
    check("a deniable cell exists to click", false);
  }

  await goto("rbac");
  await click(byText("Model grants"));
  const grantCell = $("button").find((b) => b.textContent.trim() === "·");
  if (grantCell) {
    await click(grantCell);
    const call = lastCall("POST", "/permissions");
    check(
      "a model grant is namespaced model/<name>",
      call?.body?.verb === "model:invoke" && call?.body?.resource?.startsWith("model/"),
      `resource was ${JSON.stringify(call?.body?.resource)}`,
    );
  }
  // The blanket grant is `model/*`; a role holding it must not show its models
  // as ungranted.
  check(
    "a role with model/* is marked as covering all models",
    document.getElementById("root").textContent.includes("all models"),
  );
}

// Limits: the PUT that replaced the row and wiped the other cap.
await goto("limits");
{
  // Scoped to the rate-limit card: the budget form above it has a principal
  // select too, and picking the first match filled that one instead — the
  // harness then clicked "Set" with nothing selected and reported the UI
  // broken.
  const limitCard = $("section").find((el) => el.textContent.includes("Rate limits"));
  const principal = [...limitCard.querySelectorAll("select")].find((s) =>
    [...s.options].some((o) => o.textContent.includes("batch-etl")),
  );
  const opt = [...principal.options].find((o) => o.textContent.includes("batch-etl"));
  await fill(principal, opt.value, "the rate-limit principal select");
  const reqBox = [...limitCard.querySelectorAll("input")].find(
    (i) => i.placeholder === "req/min",
  );
  check(
    "selecting a principal loads its existing limits into the form",
    reqBox && reqBox.value === "600",
    `req/min box held ${JSON.stringify(reqBox?.value)} instead of the configured 600`,
  );
  await click(
    [...limitCard.querySelectorAll("button")].find((b) => b.textContent.trim() === "Set"),
    "the rate-limit Set button",
  );
  const call = lastCall("PUT", "/limits");
  check(
    "the PUT carries both caps, not just the edited one",
    call?.body?.requests_per_min === 600 && call?.body?.tokens_per_min === 400000,
    `body was ${JSON.stringify(call?.body)}`,
  );
}

// Settings: the two danger-zone actions.
await goto("settings");
{
  await click(byText("Force snapshot rebuild"));
  check("rebuild POSTs the right path", !!lastCall("POST", "/admin/snapshot/rebuild"));
  await click(byText("Revoke all sessions"));
  check("revoke-all POSTs the right path", !!lastCall("POST", "/admin/sessions/revoke-all"));
}

// Overview's history chart, and the modal behind it. The chart itself is a
// read, so what is worth asserting is the *request* it makes and the
// controls that change it -- a chart that silently asks for the wrong window
// looks perfectly plausible on screen.
await goto("overview");
{
  const call = lastCall("GET", "/admin/timeseries");
  check("the history chart asks for a bucketed window", !!call, "no /admin/timeseries request");
  check(
    "and bounds it with since and until rather than fetching everything",
    /since=/.test(call?.path || "") && /until=/.test(call?.path || ""),
    `path was ${call?.path}`,
  );

  await click(byText("expand"));
  check(
    "expand opens the drill-down",
    !!byText("Traffic over time", "div") && !!byText("close"),
    "modal did not open",
  );

  // A range chip must move the window, not just the label.
  await click(byText("7d"));
  const wide = lastCall("GET", "/admin/timeseries");
  check(
    "a range chip asks for a wider window",
    /bucket=3600/.test(wide?.path || ""),
    `path was ${wide?.path}`,
  );

  // Panning must keep the span and move the end -- the failure mode is a
  // control that zooms out instead, which looks like it worked.
  const before = new URL(`http://x${wide.path}`).searchParams;
  await click(byText("← older"));
  const older = lastCall("GET", "/admin/timeseries");
  const after = new URL(`http://x${older.path}`).searchParams;
  const spanOf = (p) => Date.parse(p.get("until")) - Date.parse(p.get("since"));
  check(
    "panning back keeps the span and moves the window",
    spanOf(after) === spanOf(before) && Date.parse(after.get("until")) < Date.parse(before.get("until")),
    `span ${spanOf(before)} -> ${spanOf(after)}`,
  );

  await click(byText("close"));
}

// Metrics carries the same history card and the same modal. Asserted
// separately from Overview's because they are two mounts of it, and a
// component that only works on one screen looks fine on the other until
// somebody clicks.
await goto("metrics");
{
  check(
    "the metrics screen has a history chart",
    !!byText("Last 24 hours", "div"),
    "no Last 24 hours card on Metrics",
  );
  const before = lastCall("GET", "/admin/timeseries");
  check("and it asks for a bucketed window", !!before, "no /admin/timeseries request");
  await click(byText("expand"));
  check(
    "expand opens the drill-down from Metrics too",
    !!byText("Traffic over time", "div") && !!byText("close"),
    "modal did not open from Metrics",
  );
  await click(byText("close"));
}

// Usage and Audit: the read filters that change the query, not the body.
await goto("usage");
{
  await click(byText("By model"));
  const call = lastCall("GET", "group_by=model");
  check("a grouping chip changes group_by", !!call, "no request used group_by=model");
  await click(byText("24h"));
  check("a range chip sends since=", !!lastCall("GET", "since="));
}
await goto("audit");
{
  await click(byText("keys"));
  check("an audit filter sends target=", !!lastCall("GET", "target="));
  const older = containing("older");
  if (older) {
    await click(older);
    check("pagination is keyset, on before=", !!lastCall("GET", "before="));
  }
}

// The deployment screen: what it PATCHes, and what it must not.
await goto("deployment");
{
  sent.length = 0;
  const fieldNamed = (label) =>
    $("label")
      .find((l) => l.textContent.includes(label))
      ?.querySelector("input, select");

  await fill(fieldNamed("GATEWAY REPLICAS"), "6");
  await click(byText("Apply to the cluster"));
  const call = lastCall("PATCH", "/admin/deployment");
  check("scaling sends a PATCH to /admin/deployment", !!call, "no PATCH was sent");
  check(
    "replicas is sent as a number",
    call?.body?.replicas === 6,
    `sent ${JSON.stringify(call?.body?.replicas)}`,
  );
  // The reason a merge patch is used at all: a body carrying every field
  // would rewrite values managed in Git and roll the deployment for changes
  // nobody made.
  check(
    "and nothing else is",
    call && Object.keys(call.body).length === 1,
    `sent ${JSON.stringify(call?.body)}`,
  );

  // An image change is the one edit with an ordering consequence, so it must
  // arrive as `image` at the top level rather than under `proxy`.
  sent.length = 0;
  await goto("deployment");
  await fill(fieldNamed("IMAGE"), "ghcr.io/azrtydxb/fastllm-proxy:v0.4.0");
  await click(byText("Apply to the cluster"));
  const upgrade = lastCall("PATCH", "/admin/deployment");
  check(
    "an image change is sent as image",
    upgrade?.body?.image === "ghcr.io/azrtydxb/fastllm-proxy:v0.4.0",
    `sent ${JSON.stringify(upgrade?.body)}`,
  );

  // Nothing typed, nothing sent: an empty patch still bumps the resource's
  // generation, which is a rollout for no reason.
  sent.length = 0;
  await goto("deployment");
  await click(byText("Apply to the cluster"));
  check(
    "an untouched form sends nothing",
    !lastCall("PATCH", "/admin/deployment"),
    "a PATCH was sent for a form nobody edited",
  );
}

// Logout last, on purpose: it is a real control and must work, but clicking
// it mid-run invalidates every screen after it.
await goto("overview");
await click(byText("exit"), "the logout button");
check(
  "exit logs out and returns the login form",
  document.getElementById("root").textContent.includes("Sign in"),
);
check("logout POSTs /logout", !!lastCall("POST", "/logout"));

console.log(`\n  ${sent.length} requests recorded`);
await vite.close();
if (failures > 0) {
  console.log(`\n${failures} interaction check(s) failed`);
  process.exit(1);
}
console.log("\nall interactions behaved");
process.exit(0);
