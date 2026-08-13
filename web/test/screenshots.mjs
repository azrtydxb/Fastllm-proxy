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

async function shot(name, screen, prepare) {
  await page.evaluate((s) => {
    const b = [...document.querySelectorAll("nav button")].find(
      (x) => x.textContent.trim() === s,
    );
    if (b) b.click();
  }, screen);
  await settle();
  if (prepare) {
    await page.evaluate(prepare);
    await settle();
  }
  const file = `${OUT}/ui-${name}.png`;
  await page.screenshot({ path: file });
  console.log(`  ${name.padEnd(16)} ${screen}`);
}

await shot("overview", "Overview");
await shot("metrics", "Metrics");
await shot("usage", "Usage & spend");
await shot("models", "Models");
await shot("virtual-models", "Virtual models");
await shot("prompt-classes", "Prompt classes");
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
