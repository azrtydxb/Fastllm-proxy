// Capture the management UI for the documentation.
//
// Screenshots of a UI go stale silently: the page changes, the picture does
// not, and a reader trusts the picture. So these are generated from a real
// running control plane by a script that lives in the repo, rather than
// pasted in by hand — regenerating them is one command, which is the only
// thing that makes keeping them current realistic.
//
//   CONTROL_URL=https://192.168.10.129:4001 ADMIN_PASSWORD=... \
//     node test/screenshots.mjs
//
// Writes to docs/images/ui-*.png.
import { launch } from "puppeteer-core";
import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = resolve(HERE, "../../docs/images");
const CHROME =
  process.env.CHROME_PATH || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const URL = process.env.CONTROL_URL || "https://127.0.0.1:4001";
const USER = process.env.ADMIN_USER || "bootstrap";
const PASSWORD = process.env.ADMIN_PASSWORD;

if (!PASSWORD) {
  console.error("set ADMIN_PASSWORD (and CONTROL_URL) — this drives a real control plane");
  process.exit(1);
}
mkdirSync(OUT, { recursive: true });

const browser = await launch({
  executablePath: CHROME,
  // The admin plane uses a private CA. Ignoring that here is safe in a way it
  // would not be in a browser you also bank in: this instance exists only to
  // photograph a UI, in its own profile, and is thrown away at the end.
  args: ["--ignore-certificate-errors", "--user-data-dir=/tmp/fastllm-shots"],
  headless: "new",
});

const page = await browser.newPage();
// 2x for legibility when the image is scaled down in a docs page. A 1x
// screenshot of a dark UI turns to mush at half width.
await page.setViewport({ width: 1440, height: 900, deviceScaleFactor: 2 });

const settle = (ms = 1400) => new Promise((r) => setTimeout(r, ms));

await page.goto(URL, { waitUntil: "networkidle2" });

// Log in if the form is showing.
if (await page.$("form input[type=password]")) {
  const inputs = await page.$$("form input");
  await inputs[0].type(USER);
  await inputs[1].type(PASSWORD);
  await page.click("form button");
  await settle(2500);
}

async function shot(name, screen, prepare, after = 1400, finalize) {
  // Throws rather than photographing whatever was already on screen.
  //
  // `if (b) b.click()` silently produced a `ui-mcp.png` showing Prompt
  // classes, because the control plane being photographed was running an
  // image that predated the screen. A screenshot of the wrong screen is worse
  // than a missing one: it ships to the docs looking plausible.
  const found = await page.evaluate((s) => {
    const b = [...document.querySelectorAll("nav button")].find(
      (x) => x.textContent.trim() === s,
    );
    if (!b) return false;
    b.click();
    return true;
  }, screen);
  if (!found) {
    throw new Error(
      `no nav button named ${JSON.stringify(screen)} at ${URL} — is that ` +
        `control plane running an image with this screen?`,
    );
  }
  await settle();
  if (prepare) {
    await page.evaluate(prepare);
    await settle(after);
  }
  // Runs after the wait, for anything that has to act on what the wait
  // produced — scrolling to a result that did not exist when `prepare` ran.
  if (finalize) {
    await page.evaluate(finalize);
    await settle(600);
  }
  const file = `${OUT}/ui-${name}.png`;
  await page.screenshot({ path: file });
  console.log(`  ${name.padEnd(16)} ${screen}`);
}

await shot("overview", "Overview");
await shot("metrics", "Metrics");
await shot("usage", "Usage & spend");
await shot("providers", "Providers");
await shot("models", "Models");
await shot("virtual-models", "Virtual models");
await shot("prompt-classes", "Prompt classes");
await shot("mcp", "MCP servers");
await shot("agents", "Agents");
await shot("keys", "API keys");
await shot("rbac", "Principals & roles");
await shot("limits", "Limits & budgets");
await shot("audit", "Audit log");
await shot("fleet", "Fleet");
await shot("settings", "Settings");

// The drill-down is the thing a static list of screens cannot show.
await shot("timeseries-modal", "Overview", () => {
  const b = [...document.querySelectorAll("button")].find(
    (x) => x.textContent.trim() === "expand",
  );
  if (b) b.click();
});

// Semantic routing is configured in two places and the classifier chapter
// needs both: how good the classes turned out, and where a class is defined.

// Read-only: scoring examples against centroids that exclude them changes
// nothing. It is also the slowest thing on the page, hence the longer wait,
// and it renders below the table — so scroll to it or photograph the fold.
await shot(
  "prompt-class-eval",
  "Prompt classes",
  () => {
    const b = [...document.querySelectorAll("button")].find(
      (x) => x.textContent.trim() === "Run evaluation",
    );
    if (b) b.click();
  },
  8000,
  () => {
    const h = [...document.querySelectorAll("*")].find((x) =>
      x.textContent.trim().startsWith("Leave-one-out evaluation"),
    );
    if (h) h.scrollIntoView({ block: "center" });
  },
);

// React owns these inputs, so assigning `.value` is not enough — the value
// property is shadowed by React's own descriptor and the component never
// hears about the change. Go through the prototype setter and dispatch the
// event React is actually listening for. Nothing is submitted: this
// photographs the form, it does not create a class.
await shot("prompt-class-new", "Prompt classes", () => {
  const type = (el, value) => {
    const proto = Object.getPrototypeOf(el);
    Object.getOwnPropertyDescriptor(proto, "value").set.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  };
  const form = document.querySelector("form");
  type(form.querySelector("input"), "translation");
  type(
    form.querySelector("textarea"),
    [
      "Translate this paragraph into French",
      "How do you say 'where is the station' in Japanese?",
      "Render the following into formal Spanish",
      "What's the German for a quarterly earnings report?",
    ].join("\n"),
  );
});

// And the permission matrix, which is the clearest single picture of the
// RBAC model.
await shot("permission-matrix", "Principals & roles", () => {
  const b = [...document.querySelectorAll("button")].find(
    (x) => x.textContent.trim() === "Permission matrix",
  );
  if (b) b.click();
});

await browser.close();
console.log(`\n  written to ${OUT}`);
