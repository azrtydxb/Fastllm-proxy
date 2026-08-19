// Mount every screen against a stubbed admin API and fail on anything the
// browser would have logged as an error.
//
// # Why this exists
//
// `vite build` proves the modules parse and every named import resolves. It
// proves nothing about whether a screen *renders* — a component used but not
// imported, a field read off an object the API does not return, a `.map` on
// something that is null are all clean builds and blank pages. Three of those
// were in the first draft of this UI.
//
// So this walks every screen the way an operator would, with the API
// answering the shapes `src/control/api.rs` actually serialises, and treats a
// console error or a thrown render as a failure. It is not a substitute for
// looking at the thing; it is the check that the thing comes up at all.
//
// Run with `npm test` from `web/`.

import { JSDOM } from "jsdom";
import { FIXTURES, USAGE, fixtureFor } from "./fixtures.mjs";

const dom = new JSDOM(
  '<!doctype html><html><body><div id="root"></div></body></html>',
  {
    url: "http://localhost/",
    pretendToBeVisual: true,
  },
);

// Errors are collected, not forwarded: jsdom's console routes through a
// VirtualConsole that calls back into this same function, and forwarding
// recurses until the stack gives out.
const problems = [];
dom.window.console.error = (...args) => {
  problems.push(args.map(String).join(" "));
};

globalThis.window = dom.window;
globalThis.document = dom.window.document;
// Node 21+ defines a getter-only `navigator`, so it is redefined rather than
// assigned; React reads it during hydration.
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
// React only lets `act` flush effects when it is told it is in a test
// environment; without this every screen renders its loading state and the
// harness passes on a set of blank pages.
globalThis.IS_REACT_ACT_ENVIRONMENT = true;

const requested = new Set();
globalThis.fetch = async (path, init = {}) => {
  requested.add(`${init.method || "GET"} ${path}`);
  const body = fixtureFor(path);
  if (body === null && !path.startsWith("/login")) {
    // An unstubbed route is a failure of this harness, not of the UI, and it
    // must be loud — silently answering 404 would let a screen render its
    // error state and call that a pass.
    problems.push(`unstubbed route: ${path}`);
  }
  return {
    ok: true,
    status: 200,
    headers: { get: () => "application/json" },
    json: async () => (body === null ? {} : body),
  };
};

// Loaded through Vite rather than Node's own loader: the app is JSX, and this
// way the harness runs the same transform the browser bundle does instead of a
// second one that could differ.
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

// Each screen with something it must actually say. A length check alone
// passes on a page that rendered its own error state, which is exactly the
// failure worth catching — several of these strings only appear if the screen
// read the fixture correctly.
const SCREENS = [
  // The fixture has proxy-2 on an older snapshot and disagreeing about a
  // backend; both banners must appear rather than being averaged away.
  // "openai" only appears in the protocol column, which is populated by
  // joining the health report onto the model configuration. That join is built
  // from a shared key function; when the two sides built it separately, one
  // disagreed about the separator and every cell silently read "—".
  [
    "overview",
    ["serving an older snapshot", "Replicas disagree", "BACKENDS UP", "openai"],
  ],
  ["metrics", ["Response cache", "Latency percentiles", "histogram_quantile"]],
  // The `mixed` row is partly unpriced and the `audit@kryton` row entirely so.
  ["usage", ["unpriced", "Spend by model", "COST / 1M TOKENS"]],
  ["providers", ["10.42.1.7", "api.anthropic.com"]],
  ["models", ["local-qwen", "claude-sonnet", "unpriced", "cache 300s"]],
  // "engineering" and "batch" only appear inside condition chips, so this
  // fails if the conditions are read from the wrong shape again.
  [
    "routing",
    ["gpt-router", "class =", "coding", "engineering", "batch", "Defaults"],
  ],
  ["classes", ["coding", "no centroid", "cannot route"]],
  // A disabled server, one with no credential, and the namespacing note: the
  // three things this screen exists to make visible.
  [
    "mcp",
    ["github", "internal-wiki", "disabled", "no credential", "server__tool"],
  ],
  // The pinned version and the card rewrite are the two things this screen
  // exists to make visible.
  [
    "agents",
    [
      "planner",
      "deployer",
      "0.3",
      "1.0",
      "disabled",
      "rewritten to point at this gateway",
    ],
  ],
  ["keys", ["ci-runner", "revoked", "sk-abcd1234"]],
  ["rbac", ["ops@kryton", "batch-etl", "admin"]],
  // $155.00 of a $500.00 cap. Formatted money must keep its cents: dropping
  // them above $100 — where the caps are — made a budget at 99.9% read as
  // exactly at its cap.
  ["limits", ["batch-etl", "402", "429", "no spend cap", "$500.00", "$155.00"]],
  ["audit", ["/admin/keys", "DELETE", "Reads are not recorded"]],
  // A snapshot version is epoch microseconds, so the banner reports how far
  // behind the laggard is rather than printing sixteen digits at somebody.
  [
    "fleet",
    [
      "proxy-1",
      "proxy-2 (4m behind)",
      "stuck on an older snapshot",
      "USAGE DROPPED",
    ],
  ],
  ["settings", ["Deployment-wide fallback", "Danger zone", "12h", "fast only"]],
  // The operator screen: the held image, the phase, and the sentence
  // explaining why the two differ.
  [
    "deployment",
    [
      "fastllm/fastllm",
      "Upgrading",
      "gateway held at",
      "ghcr.io/azrtydxb/fastllm-proxy:v0.2.0",
      "Autoscaling",
      "Apply to the cluster",
    ],
  ],
];

const root = createRoot(document.getElementById("root"));

async function settle() {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 30));
  });
}

await act(async () => {
  root.render(React.createElement(App));
});
await settle();

let failures = 0;
for (const [screen, expected] of SCREENS) {
  problems.length = 0;
  // A screen that throws while rendering takes React's whole root down, so it
  // is caught here and reported as that screen's failure — otherwise the run
  // ends in a raw stack trace and the remaining screens are never checked.
  let threw = null;
  try {
    await act(async () => {
      dom.window.location.hash = `#/${screen}`;
      dom.window.dispatchEvent(new dom.window.Event("hashchange"));
    });
    await settle();
  } catch (e) {
    threw = e;
  }

  const text = document.getElementById("root").textContent || "";
  const real = problems.filter(
    // React's act() advice and jsdom's unimplemented-CSS noise are about the
    // harness, not the screen.
    (p) => !/not wrapped in act|Not implemented|unknown prop/i.test(p),
  );
  const missing = threw ? [] : expected.filter((e) => !text.includes(e));
  const ok = !threw && real.length === 0 && missing.length === 0;
  if (!ok) {
    failures++;
    console.log(`  ${screen}: FAILED`);
    if (threw) console.log(`      threw: ${threw.message}`);
    for (const p of real.slice(0, 3))
      console.log(`      error: ${p.split("\n")[0]}`);
    for (const m of missing)
      console.log(`      missing from the page: ${JSON.stringify(m)}`);
  } else {
    console.log(`  ${screen}: ok (${text.length} chars)`);
    if (process.env.DUMP === screen)
      console.log("\n----\n" + text + "\n----\n");
  }
}

// Invisible without an operator.
//
// The whole contract of the deployment screen is that a Helm or manifest
// install never sees it: the routes behind it 404 there, so an entry that
// merely looked greyed out would be a control that cannot work. This checks
// both directions off the same capability flag the shell reads.
{
  const { visibleNav } = await vite.ssrLoadModule("/src/App.jsx");
  const labels = (config) =>
    visibleNav(config)
      .flatMap((g) => g.items)
      .map((i) => i.id);

  const managed = labels({ operator_managed: true });
  const unmanaged = labels({ operator_managed: false });
  const missing = labels(null);

  if (!managed.includes("deployment")) {
    failures++;
    console.log(
      "  nav/operator: FAILED (no Deployment entry under an operator)",
    );
  } else if (
    unmanaged.includes("deployment") ||
    missing.includes("deployment")
  ) {
    failures++;
    console.log(
      "  nav/operator: FAILED (Deployment entry shown without an operator)",
    );
  } else if (unmanaged.length !== managed.length - 1) {
    failures++;
    console.log(
      "  nav/operator: FAILED (the flag hid more than the one screen)",
    );
  } else {
    console.log(
      `  nav/operator: ok (${managed.length} screens managed, ${unmanaged.length} not)`,
    );
  }
}

// The RBAC screen's other two tabs are separate render paths behind a button,
// and the permission matrix is the most intricate thing here.
problems.length = 0;
await act(async () => {
  dom.window.location.hash = "#/rbac";
  dom.window.dispatchEvent(new dom.window.Event("hashchange"));
});
await settle();
for (const label of ["Permission matrix", "Model grants"]) {
  const button = [...document.querySelectorAll("button")].find(
    (b) => b.textContent.trim() === label,
  );
  if (!button) {
    console.log(`  rbac/${label}: FAILED (no tab button)`);
    failures++;
    continue;
  }
  await act(async () => {
    button.dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
  });
  await settle();
  const cells = document.querySelectorAll("button").length;
  console.log(`  rbac/${label}: ok (${cells} buttons)`);
}

console.log(`\n  ${requested.size} distinct API calls made`);
if (failures > 0) {
  console.log(`\n${failures} screen(s) failed to render`);
  process.exit(1);
}
console.log("\nall screens rendered");
await vite.close();
process.exit(0);
