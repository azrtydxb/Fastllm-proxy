// Drive the real UI in a real browser, against a real control plane.
//
// # Why this exists on top of the other two harnesses
//
// `render.mjs` and `interact.mjs` run in jsdom, which parses HTML and runs
// JavaScript but does **no layout at all**: every element is zero by zero, no
// CSS is resolved, nothing overlaps because nothing has a position. So the
// entire visual half of a UI — the half this project's design work was about —
// was unverified by them. They cannot see a column that overflows its grid,
// text clipped by a fixed height, a control rendered underneath another, or a
// panel pushed off-screen. Neither can they catch a screen that works against
// a fixture and fails against the API.
//
// This runs the built bundle in headless Chrome, logs in for real, walks every
// screen, and checks the things only a browser knows:
//
// - console errors and failed requests,
// - horizontal overflow of the document,
// - controls smaller than a usable size, or with zero area,
// - text overflowing its container,
// - and a screenshot of each screen, written to disk to be looked at.
//
// It launches Chrome with its own `--user-data-dir`, so it never touches a
// browser the developer already has open — which is what blocked this from
// being done in the first place.
//
//   FASTLLM_UI_URL=http://127.0.0.1:18091 \
//   FASTLLM_ADMIN_USER=bootstrap FASTLLM_ADMIN_PASSWORD=... \
//   node test/browser.mjs

import { mkdirSync, rmSync } from "node:fs";
import { launch } from "puppeteer-core";

const url = process.env.FASTLLM_UI_URL;
if (!url) {
  console.log("browser: no FASTLLM_UI_URL — skipped");
  process.exit(0);
}
const user = process.env.FASTLLM_ADMIN_USER || "bootstrap";
const password = process.env.FASTLLM_ADMIN_PASSWORD;
if (!password) {
  console.error("browser: FASTLLM_ADMIN_PASSWORD is required");
  process.exit(2);
}

const CHROME =
  process.env.CHROME_PATH || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const SHOTS = new URL("../.screenshots/", import.meta.url).pathname;
rmSync(SHOTS, { recursive: true, force: true });
mkdirSync(SHOTS, { recursive: true });

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
  "fleet",
  // Only exists under an operator, so it is walked only when the control
  // plane this is pointed at says so — checked below rather than assumed,
  // because asserting it unconditionally would fail every Helm deployment
  // and asserting it never would test nothing.
  "deployment",
  "settings",
];

const browser = await launch({
  executablePath: CHROME,
  headless: true,
  // Its own profile directory: the developer's Chrome stays untouched, which
  // is the whole reason this can run at all.
  userDataDir: `${SHOTS}.profile`,
  args: ["--no-sandbox", "--disable-dev-shm-usage", "--ignore-certificate-errors"],
});

let failures = 0;
const note = (ok, name, detail) => {
  if (!ok) failures++;
  console.log(`  ${ok ? "✓" : "✗"} ${name}${detail ? ` — ${detail}` : ""}`);
};

const page = await browser.newPage();
await page.setViewport({ width: 1440, height: 900 });

const consoleErrors = [];
const failedRequests = [];
page.on("console", (m) => {
  if (m.type() === "error") consoleErrors.push(m.text());
});
page.on("pageerror", (e) => consoleErrors.push(`uncaught: ${e.message}`));
page.on("requestfailed", (r) => failedRequests.push(`${r.method()} ${r.url()}`));
page.on("response", (r) => {
  if (r.status() >= 500) failedRequests.push(`${r.status()} ${r.url()}`);
});

console.log(`browser: ${url}`);

// --- login, for real -------------------------------------------------------

await page.goto(url, { waitUntil: "networkidle2" });
await page.screenshot({ path: `${SHOTS}00-login.png` });
const inputs = await page.$$("input");
note(inputs.length >= 2, "login form renders two fields");
await inputs[0].type(user);
await inputs[1].type(password);
await Promise.all([
  page.$$eval("button", (bs) => bs.find((b) => b.textContent.includes("Sign in")).click()),
  page.waitForFunction(() => !document.body.textContent.includes("Sign in"), { timeout: 15000 }),
]);
note(true, "signed in against the real control plane");

/** What only a browser can answer. */
async function inspect() {
  return page.evaluate(() => {
    const problems = [];
    const de = document.documentElement;
    if (de.scrollWidth > de.clientWidth + 1) {
      problems.push(`page scrolls sideways: ${de.scrollWidth} > ${de.clientWidth}`);
    }
    const visible = (el) => {
      const s = getComputedStyle(el);
      return s.display !== "none" && s.visibility !== "hidden" && s.opacity !== "0";
    };
    // Controls a person has to hit. Anything with no area is unclickable, and
    // anything under ~16px tall in a dense admin UI is a mis-render.
    for (const el of document.querySelectorAll("button, input, select")) {
      if (!visible(el)) continue;
      const r = el.getBoundingClientRect();
      const label = (el.textContent || el.placeholder || el.tagName).trim().slice(0, 30);
      if (r.width === 0 || r.height === 0) problems.push(`zero-size control: "${label}"`);
      else if (r.height < 12) problems.push(`control ${r.height.toFixed(0)}px tall: "${label}"`);
    }
    // Text wider than the box that holds it, without the box being told to
    // scroll or ellipsise it.
    for (const el of document.querySelectorAll("main *")) {
      if (!visible(el) || el.children.length) continue;
      const s = getComputedStyle(el);
      if (s.overflow !== "visible" || s.textOverflow === "ellipsis") continue;
      if (el.scrollWidth > el.clientWidth + 2 && el.clientWidth > 0) {
        problems.push(`text overflows: "${(el.textContent || "").trim().slice(0, 40)}"`);
      }
    }
    return problems;
  });
}

// --- every screen ----------------------------------------------------------

// Whether this deployment has an operator, asked of the control plane rather
// than assumed. The deployment screen exists only there.
const managed = await page.evaluate(async () => {
  const r = await fetch("/admin/config", { credentials: "same-origin" });
  return r.ok ? Boolean((await r.json()).operator_managed) : false;
});
console.log(`  (operator-managed: ${managed})`);

for (const [i, screen] of SCREENS.entries()) {
  if (screen === "deployment" && !managed) {
    // And prove it is *absent*, not merely unvisited: the nav must not offer
    // a screen whose every request would 404.
    const offered = await page.evaluate(() =>
      [...document.querySelectorAll("nav button")].some((b) =>
        b.textContent.includes("Deployment"),
      ),
    );
    note(!offered, "deployment screen hidden without an operator", offered && "nav offers it");
    continue;
  }
  consoleErrors.length = 0;
  failedRequests.length = 0;
  await page.goto(`${url}/#/${screen}`, { waitUntil: "networkidle2" });
  // Polling screens settle a beat after the first paint.
  await new Promise((r) => setTimeout(r, 900));
  await page.screenshot({ path: `${SHOTS}${String(i + 1).padStart(2, "0")}-${screen}.png` });

  const text = await page.evaluate(() => document.querySelector("main")?.innerText || "");
  const problems = await inspect();
  const errs = consoleErrors.filter((e) => !/favicon/i.test(e));
  const fails = failedRequests.filter((r) => !/favicon/i.test(r));

  note(
    errs.length === 0 && fails.length === 0 && problems.length === 0 && text.length > 50,
    `${screen} (${text.length} chars of copy)`,
    [errs[0], fails[0], problems.slice(0, 3).join("; "), text.length <= 50 && "almost no content"]
      .filter(Boolean)
      .join(" | ") || undefined,
  );
}

// --- a real interaction, end to end ---------------------------------------

// The dry-run is the one control whose whole value is the answer it prints, so
// it is worth driving against the live routing engine rather than a stub.
await page.goto(`${url}/#/routing`, { waitUntil: "networkidle2" });
await new Promise((r) => setTimeout(r, 600));
const hasVm = await page.evaluate(() =>
  [...document.querySelectorAll("button")].some((b) => b.textContent.includes("Dry-run")),
);
if (hasVm) {
  await page.$$eval("button", (bs) => bs.find((b) => b.textContent.includes("Dry-run")).click());
  await new Promise((r) => setTimeout(r, 300));
  await page.$$eval("button", (bs) => bs.find((b) => b.textContent.trim() === "Evaluate").click());
  await new Promise((r) => setTimeout(r, 1200));
  const answered = await page.evaluate(() => {
    const t = document.querySelector("main").innerText;
    return t.includes("RULE") || t.includes("NO RULE");
  });
  note(answered, "dry-run answers from the live routing engine");
  await page.screenshot({ path: `${SHOTS}14-dryrun.png` });
} else {
  console.log("  — no virtual model configured; dry-run not exercised");
}

// The operator screen, driven for real: read the resource, change nothing,
// and confirm the page is showing live status rather than an empty form.
if (managed) {
  await page.goto(`${url}/#/deployment`, { waitUntil: "networkidle2" });
  await new Promise((r) => setTimeout(r, 900));
  const state = await page.evaluate(() => {
    const t = document.querySelector("main").innerText;
    const inputs = [...document.querySelectorAll("input")].filter((i) => i.type !== "checkbox");
    return {
      text: t,
      filled: inputs.filter((i) => i.value && i.value.length).length,
      apply: [...document.querySelectorAll("button")].find((b) =>
        b.textContent.includes("Apply to the cluster"),
      )?.disabled,
    };
  });
  note(state.filled >= 3, "the form is populated from the live resource", `${state.filled} fields`);
  note(state.text.includes("SERVING"), "it reports what is actually serving");
  note(state.apply === true, "Apply is disabled until something is edited");

  // A nav entry that exists, and reaches the screen.
  const inNav = await page.evaluate(() =>
    [...document.querySelectorAll("nav button")].some((b) => b.textContent.includes("Deployment")),
  );
  note(inNav, "the Deployment entry is in the nav under an operator");

  // Editing arms the button, discarding disarms it. Deliberately stops short
  // of clicking Apply: this harness is pointed at whatever deployment it is
  // given, and a browser test that scaled somebody's gateway to prove it can
  // would be a bad trade. The write path is covered by `interact.mjs`, which
  // asserts the exact body this button sends, and end to end against the
  // admin API.
  const armed = await page.evaluate(() => {
    const input = [...document.querySelectorAll("label")]
      .find((l) => l.textContent.includes("GATEWAY REPLICAS"))
      ?.querySelector("input");
    if (!input) return null;
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype,
      "value",
    ).set;
    setter.call(input, "5");
    input.dispatchEvent(new Event("input", { bubbles: true }));
    return true;
  });
  await new Promise((r) => setTimeout(r, 400));
  const afterEdit = await page.evaluate(
    () =>
      [...document.querySelectorAll("button")].find((b) =>
        b.textContent.includes("Apply to the cluster"),
      )?.disabled,
  );
  note(armed && afterEdit === false, "editing a field arms Apply");

  await page.$$eval("button", (bs) => bs.find((b) => b.textContent.trim() === "Discard")?.click());
  await new Promise((r) => setTimeout(r, 400));
  const afterDiscard = await page.evaluate(
    () =>
      [...document.querySelectorAll("button")].find((b) =>
        b.textContent.includes("Apply to the cluster"),
      )?.disabled,
  );
  note(afterDiscard === true, "Discard puts the form back to the live values");

  await page.screenshot({ path: `${SHOTS}15-deployment.png` });
}

// The 1280 width a laptop actually uses, to catch a layout that only works at
// the 1440 the design was drawn at.
await page.setViewport({ width: 1280, height: 800 });
for (const screen of ["overview", "models", "rbac", "limits"]) {
  await page.goto(`${url}/#/${screen}`, { waitUntil: "networkidle2" });
  await new Promise((r) => setTimeout(r, 700));
  const problems = await inspect();
  note(problems.length === 0, `${screen} at 1280px`, problems.slice(0, 2).join("; "));
  await page.screenshot({ path: `${SHOTS}narrow-${screen}.png` });
}

await browser.close();
console.log(`\n  screenshots in web/.screenshots/`);
if (failures) {
  console.log(`\n${failures} browser check(s) failed`);
  process.exit(1);
}
console.log("\nthe UI works in a real browser");
process.exit(0);
