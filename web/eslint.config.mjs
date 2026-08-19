// Minimal, dependency-free lint config: espree parses JSX natively, so the
// screens get linted at all (without one, eslint skips every .jsx file), and
// the browser globals stop being reported as undefined. Deliberately no
// plugins — the repo's web toolchain is vite + react and nothing else.
const browserGlobals = Object.fromEntries(
  [
    "window",
    "document",
    "navigator",
    "location",
    "history",
    "fetch",
    "console",
    "alert",
    "confirm",
    "prompt",
    "setTimeout",
    "clearTimeout",
    "setInterval",
    "clearInterval",
    "requestAnimationFrame",
    "localStorage",
    "sessionStorage",
    "URL",
    "URLSearchParams",
    "FormData",
    "Blob",
    "File",
    "AbortController",
    "WebSocket",
    "EventSource",
    "Event",
    "CustomEvent",
    "KeyboardEvent",
    "MouseEvent",
    "getComputedStyle",
    "crypto",
    "performance",
    "atob",
    "btoa",
    "TextDecoder",
    "TextEncoder",
    "structuredClone",
  ].map((name) => [name, "readonly"]),
);

export default [
  {
    files: ["**/*.{js,jsx,mjs}"],
    ignores: ["dist/**", "node_modules/**"],
    languageOptions: {
      ecmaVersion: "latest",
      sourceType: "module",
      parserOptions: { ecmaFeatures: { jsx: true } },
      globals: {
        ...browserGlobals,
        process: "readonly", // vite injects import.meta/process.env shims
      },
    },
    rules: {
      "no-undef": "error",
      "no-unused-vars": ["error", { argsIgnorePattern: "^_" }],
    },
  },
  {
    // The node test harnesses drive jsdom/puppeteer from node itself.
    files: ["test/**/*.mjs", "vite.config.js"],
    languageOptions: {
      globals: {
        process: "readonly",
        Buffer: "readonly",
        __dirname: "readonly",
        global: "readonly",
      },
    },
  },
];
