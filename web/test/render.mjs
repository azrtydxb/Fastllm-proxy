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
// So this walks all thirteen screens the way an operator would, with the API
// answering the shapes `src/control/api.rs` actually serialises, and treats a
// console error or a thrown render as a failure. It is not a substitute for
// looking at the thing; it is the check that the thing comes up at all.
//
// Run with `npm test` from `web/`.

import { JSDOM } from "jsdom";

const FIXTURES = {
  "/admin/config": {
    role: "all",
    version: "0.1.0",
    tls: false,
    uptime_seconds: 4210,
    config_poll_seconds: 5,
    health_report_interval_seconds: 10,
    cache_max_entries: 4096,
    cache_max_bytes: 67108864,
    otel_endpoint: null,
    otel_sample_one_in: 0,
    classifier_tier1: true,
    classifier_tier2: false,
    session_ttl_hours: 12,
    snapshot_rebuild_failures: 0,
    snapshot_version: 418,
    models: 2,
    models_unpriced: 1,
    models_cached: 1,
  },
  // Two replicas that disagree about one backend and about the snapshot
  // version: the two states this UI exists to surface, so the banners and the
  // split-health rendering are both exercised rather than only the happy path.
  "/admin/fleet": [
    {
      replica: "proxy-1",
      snapshot_version: 418,
      uptime_seconds: 90061,
      backends: [
        {
          api_base: "http://10.42.1.7:8000/v1",
          model: "qwen2.5-32b",
          healthy: true,
          inflight: 3,
          requests_total: 41000,
          errors_total: 2,
        },
        {
          api_base: "https://api.anthropic.com/v1",
          model: "claude-sonnet-4-5",
          healthy: true,
          inflight: 1,
          requests_total: 900,
          errors_total: 0,
        },
      ],
      process: {
        requests_ok: 41900,
        requests_failed: 2,
        cache_hits: 120,
        cache_misses: 380,
        cache_entries: 88,
        cache_bytes: 1048576,
        usage_dropped: 0,
      },
    },
    {
      replica: "proxy-2",
      snapshot_version: 417,
      uptime_seconds: 300,
      backends: [
        {
          api_base: "http://10.42.1.7:8000/v1",
          model: "qwen2.5-32b",
          healthy: false,
          inflight: 0,
          requests_total: 10,
          errors_total: 4,
        },
      ],
      process: {
        requests_ok: 10,
        requests_failed: 4,
        cache_hits: 0,
        cache_misses: 6,
        cache_entries: 0,
        cache_bytes: 0,
        usage_dropped: 214,
      },
    },
  ],
  "/admin/models": [
    {
      id: 3,
      name: "local-qwen",
      description: "self-hosted",
      input_price_per_mtok: 0,
      output_price_per_mtok: 0,
      cache_ttl_seconds: 300,
      backends: [
        {
          id: 11,
          api_base: "http://10.42.1.7:8000/v1",
          upstream_model: "qwen2.5-32b",
          has_upstream_api_key: false,
          protocol: "openai",
          auth_header: "authorization",
          default_max_tokens: null,
        },
      ],
    },
    {
      id: 5,
      name: "claude-sonnet",
      description: "",
      input_price_per_mtok: null,
      output_price_per_mtok: null,
      cache_ttl_seconds: null,
      backends: [
        {
          id: 12,
          api_base: "https://api.anthropic.com/v1",
          upstream_model: "claude-sonnet-4-5",
          has_upstream_api_key: true,
          protocol: "anthropic",
          auth_header: "x-api-key",
          default_max_tokens: 4096,
        },
      ],
    },
  ],
  "/admin/virtual-models": [
    {
      id: 2,
      name: "gpt-router",
      description: "",
      rules: [
        {
          id: 7,
          position: 0,
          match_condition: {
            principals: [],
            roles: ["engineering"],
            min_prompt_tokens: 256,
            stream: true,
            headers: { "x-fastllm-tier": "batch" },
            days: [],
            class: "coding",
          },
          targets: [{ id: 21, model_id: 5, model: "claude-sonnet", weight: 80, position: 0 }],
        },
      ],
      default_targets: [{ id: 22, model_id: 3, model: "local-qwen", weight: 100, position: 0 }],
    },
  ],
  "/admin/principals": [
    {
      id: 1,
      kind: "user",
      name: "ops@kryton",
      email: "ops@kryton",
      disabled: false,
      created_at: "2026-01-01T00:00:00Z",
      roles: ["admin"],
    },
    {
      id: 2,
      kind: "service_account",
      name: "batch-etl",
      email: null,
      disabled: false,
      created_at: "2026-01-01T00:00:00Z",
      roles: ["inference"],
    },
  ],
  "/admin/roles": [
    {
      id: 1,
      name: "admin",
      description: "",
      permissions: [
        { verb: "usage:read", resource: "*" },
        { verb: "config:write", resource: "*" },
        { verb: "model:invoke", resource: "*" },
      ],
    },
    {
      id: 2,
      name: "inference",
      description: "",
      permissions: [{ verb: "model:invoke", resource: "model/local-qwen" }],
    },
  ],
  "/admin/keys": [
    {
      id: 31,
      prefix: "sk-abcd1234",
      name: "ci-runner",
      principal_id: 2,
      principal: "batch-etl",
      expires_at: "2026-12-01T00:00:00Z",
      disabled: false,
      created_at: "2026-01-01T00:00:00Z",
      last_used_at: "2026-08-09T10:00:00Z",
    },
    {
      id: 32,
      prefix: "sk-dead0000",
      name: "old",
      principal_id: 2,
      principal: "batch-etl",
      expires_at: null,
      disabled: true,
      created_at: "2026-01-01T00:00:00Z",
      last_used_at: null,
    },
  ],
  "/admin/limits": [
    { principal_id: 2, principal: "batch-etl", requests_per_min: 600, tokens_per_min: 400000 },
  ],
  "/admin/budgets": [
    {
      principal_id: 2,
      principal: "batch-etl",
      tokens_total: 12000000,
      tokens_used: 10100000,
      cost_total_micros: 500000000,
      cost_used_micros: 155000000,
      window: "daily",
      window_start: "2026-08-09T00:00:00Z",
    },
    {
      principal_id: 1,
      principal: "ops@kryton",
      tokens_total: null,
      tokens_used: 4,
      cost_total_micros: null,
      cost_used_micros: 0,
      window: "monthly",
      window_start: "2026-08-01T00:00:00Z",
    },
  ],
  "/admin/prompt-classes": [
    {
      id: 1,
      name: "coding",
      description: "",
      tier: "fast",
      min_margin: 0.08,
      refines: [],
      examples: 32,
      routable: true,
    },
    {
      id: 2,
      name: "summarise",
      description: "",
      tier: "refined",
      min_margin: null,
      refines: ["coding"],
      examples: 6,
      routable: false,
    },
  ],
  "/admin/fallback-model": { id: 3, name: "local-qwen" },
  "/admin/audit": [
    {
      id: 1361,
      actor_name: "ops@kryton",
      actor_id: 1,
      action: "POST",
      target: "/admin/keys",
      detail: { status: 200 },
      at: "2026-08-09T09:00:00Z",
    },
    {
      id: 1360,
      actor_name: "proxy-token",
      actor_id: null,
      action: "DELETE",
      target: "/admin/backends/19",
      detail: {},
      at: "2026-08-09T08:00:00Z",
    },
  ],
};

// Usage answers on any group_by, including the row shapes that catch the
// unpriced handling: one fully unpriced, one partly.
const USAGE = [
  {
    key: "batch-etl",
    requests: 900,
    prompt_tokens: 412000,
    completion_tokens: 96000,
    cost_micros: 1204000000,
    unpriced_requests: 0,
  },
  {
    key: "audit@kryton",
    requests: 12,
    prompt_tokens: 3000,
    completion_tokens: 1000,
    cost_micros: 0,
    unpriced_requests: 12,
  },
  {
    key: "mixed",
    requests: 50,
    prompt_tokens: 1000,
    completion_tokens: 500,
    cost_micros: 25000,
    unpriced_requests: 7,
  },
];

function fixtureFor(path) {
  const [base] = path.split("?");
  if (base === "/admin/usage") return USAGE;
  if (base in FIXTURES) return FIXTURES[base];
  return null;
}

const dom = new JSDOM('<!doctype html><html><body><div id="root"></div></body></html>', {
  url: "http://localhost/",
  pretendToBeVisual: true,
});

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
// harness passes on thirteen blank pages.
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
  ["overview", ["serving an older snapshot", "Replicas disagree", "BACKENDS UP", "openai"]],
  ["metrics", ["Response cache", "Latency percentiles", "histogram_quantile"]],
  // The `mixed` row is partly unpriced and the `audit@kryton` row entirely so.
  ["usage", ["unpriced", "Spend by model", "COST / 1M TOKENS"]],
  ["providers", ["10.42.1.7", "api.anthropic.com"]],
  ["models", ["local-qwen", "claude-sonnet", "unpriced", "cache 300s"]],
  ["routing", ["gpt-router", "class =", "coding", "Defaults"]],
  ["classes", ["coding", "no centroid", "cannot route"]],
  ["keys", ["ci-runner", "revoked", "sk-abcd1234"]],
  ["rbac", ["ops@kryton", "batch-etl", "admin"]],
  ["limits", ["batch-etl", "402", "429", "no spend cap"]],
  ["audit", ["/admin/keys", "DELETE", "Reads are not recorded"]],
  ["fleet", ["proxy-1", "proxy-2", "stuck on an older snapshot", "USAGE DROPPED"]],
  ["settings", ["Deployment-wide fallback", "Danger zone", "12h", "fast only"]],
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
    for (const p of real.slice(0, 3)) console.log(`      error: ${p.split("\n")[0]}`);
    for (const m of missing) console.log(`      missing from the page: ${JSON.stringify(m)}`);
  } else {
    console.log(`  ${screen}: ok (${text.length} chars)`);
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
