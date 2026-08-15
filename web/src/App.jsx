// The shell: sidebar, header, and which screen is mounted.
//
// Navigation is a `useState` and a hash, not a router. There are sixteen
// screens, no nested routes, and no parameters beyond "which one" — a router
// would be a dependency carrying its own upgrade treadmill for a `switch`.
// The hash is there so a reload, and a link pasted to a colleague, both land
// where they were.
//
// One of them is conditional: **Deployment** exists only where a Kubernetes
// operator manages this process, because it edits a `FastllmProxy` resource
// rather than the database. `visibleNav` is what keeps it out of every other
// install, and the shell falls back to Overview if a hash names it anyway.

import React, { useCallback, useEffect, useState } from "react";
import { api, ApiError } from "./api.js";
import { Button, Dot, ErrorNote, Pill } from "./ui.jsx";

import { Login } from "./views/Login.jsx";
import { Overview } from "./views/Overview.jsx";
import { Metrics } from "./views/Metrics.jsx";
import { Usage } from "./views/Usage.jsx";
import { Agents } from "./views/Agents.jsx";
import { McpServers } from "./views/McpServers.jsx";
import { Providers } from "./views/Providers.jsx";
import { Models } from "./views/Models.jsx";
import { VirtualModels } from "./views/VirtualModels.jsx";
import { PromptClasses } from "./views/PromptClasses.jsx";
import { Keys } from "./views/Keys.jsx";
import { Principals } from "./views/Principals.jsx";
import { LimitsAndBudgets } from "./views/LimitsAndBudgets.jsx";
import { Audit } from "./views/Audit.jsx";
import { Fleet } from "./views/Fleet.jsx";
import { Settings } from "./views/Settings.jsx";
import { Deployment } from "./views/Deployment.jsx";

const SCREENS = {
  overview: { title: "Overview", subtitle: "control plane, fleet and backends", view: Overview },
  metrics: {
    title: "Metrics",
    subtitle: "watched live, since this page loaded",
    view: Metrics,
  },
  usage: { title: "Usage & spend", subtitle: "folded from usage_events", view: Usage },
  providers: {
    title: "Providers",
    subtitle: "grouped by api_base — adding one is a row in a table",
    view: Providers,
  },
  models: {
    title: "Backend models",
    subtitle: "what requests are routed to — one name, one or more backends",
    view: Models,
  },
  routing: {
    title: "Frontend models",
    subtitle: "what clients ask for — rules, weights and failover chains",
    view: VirtualModels,
  },
  classes: { title: "Prompt classes", subtitle: "semantic routing · two tiers", view: PromptClasses },
  agents: {
    title: "Agents",
    subtitle: "A2A, with the card rewritten to point here",
    view: Agents,
  },
  mcp: {
    title: "MCP servers",
    subtitle: "one endpoint in front of every tool server",
    view: McpServers,
  },
  keys: { title: "API keys", subtitle: "SHA-256 hashed · prefix stored in the clear", view: Keys },
  rbac: { title: "Principals & roles", subtitle: "RBAC and per-model grants", view: Principals },
  limits: { title: "Limits & budgets", subtitle: "tokens, money, or both", view: LimitsAndBudgets },
  audit: { title: "Audit log", subtitle: "append-only · every /admin/* mutation", view: Audit },
  fleet: { title: "Fleet", subtitle: "what each proxy replica can see", view: Fleet },
  settings: { title: "Settings", subtitle: "fallback, and what this process was started with", view: Settings },
  // Only reachable under an operator — see NAV below and `operator_managed`.
  deployment: {
    title: "Deployment",
    subtitle: "the FastllmProxy an operator reconciles",
    view: Deployment,
  },
};

const NAV = [
  {
    label: "OBSERVE",
    items: [
      { id: "overview", label: "Overview" },
      { id: "metrics", label: "Metrics" },
      { id: "usage", label: "Usage & spend" },
    ],
  },
  {
    label: "ROUTE",
    items: [
      { id: "providers", label: "Providers" },
      { id: "models", label: "Backend models" },
      { id: "routing", label: "Frontend models" },
      { id: "classes", label: "Prompt classes" },
      { id: "mcp", label: "MCP servers" },
      { id: "agents", label: "Agents" },
    ],
  },
  {
    label: "ACCESS",
    items: [
      { id: "keys", label: "API keys" },
      { id: "rbac", label: "Principals & roles" },
      { id: "limits", label: "Limits & budgets" },
      { id: "audit", label: "Audit log" },
    ],
  },
  {
    label: "SYSTEM",
    items: [
      { id: "fleet", label: "Fleet" },
      // Absent unless an operator manages this deployment. Not disabled,
      // absent: a greyed-out entry invites somebody to work out how to enable
      // it, when the honest answer is that this install has nothing to show
      // there. See `visibleNav`.
      { id: "deployment", label: "Deployment", needs: "operator_managed" },
      { id: "settings", label: "Settings" },
    ],
  },
];

/** The nav minus anything this deployment cannot do. */
export function visibleNav(config) {
  return NAV.map((group) => ({
    ...group,
    items: group.items.filter((item) => !item.needs || Boolean(config?.[item.needs])),
  })).filter((group) => group.items.length > 0);
}

function screenFromHash() {
  const id = window.location.hash.replace(/^#\/?/, "");
  return SCREENS[id] ? id : "overview";
}

// `authed` starts `null` (unknown) rather than `false`, so the login form does
// not flash on screen for a returning user whose session cookie is still valid
// while the first probe request is in flight.
export function App() {
  const [authed, setAuthed] = useState(null);
  const [who, setWho] = useState(null);
  const [screen, setScreen] = useState(screenFromHash);
  const [error, setError] = useState(null);
  const [config, setConfig] = useState(null);

  // No `GET /whoami` route exists, so session validity is probed with the
  // cheapest real admin read there is: `GET /admin/config`. A 401 there means
  // "not logged in", which is exactly the question being asked — and its
  // answer is the deployment banner in the header, so the probe is not wasted.
  const probe = useCallback(async () => {
    try {
      const cfg = await api.get("/admin/config");
      setConfig(cfg);
      setAuthed(true);
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) {
        setAuthed(false);
        return;
      }
      // Anything else — a control plane that is up but unhealthy — is not an
      // authentication failure, and showing a login form for it would send an
      // operator round a loop that cannot succeed.
      setError(e.message);
      setAuthed(false);
    }
  }, []);

  useEffect(() => {
    probe();
  }, [probe]);

  useEffect(() => {
    const onHash = () => setScreen(screenFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const go = (id) => {
    window.location.hash = `#/${id}`;
    setScreen(id);
  };

  if (authed === null) return null;
  if (!authed) {
    return (
      <Login
        error={error}
        onLoggedIn={(name) => {
          setWho(name);
          setError(null);
          probe();
        }}
      />
    );
  }

  const logout = async () => {
    try {
      await api.post("/logout");
    } finally {
      setAuthed(false);
      setConfig(null);
    }
  };

  // A hash naming a screen this deployment does not have — `#/deployment` on
  // a Helm install, or a bookmark carried between clusters — falls back to
  // Overview rather than rendering a page whose every request 404s.
  const reachable =
    screen !== "deployment" || Boolean(config?.operator_managed);
  const active = reachable ? SCREENS[screen] : SCREENS.overview;
  const View = active.view;
  const onUnauthorised = () => setAuthed(false);

  return (
    <div style={{ display: "flex", height: "100%", background: "var(--bg)", color: "var(--fg)" }}>
      <aside
        style={{
          width: 228,
          flex: "none",
          borderRight: "1px solid var(--line-mid)",
          display: "flex",
          flexDirection: "column",
          background: "var(--panel-2)",
        }}
      >
        <div
          style={{
            padding: "18px 18px 16px",
            borderBottom: "1px solid var(--line-mid)",
            display: "flex",
            alignItems: "baseline",
            gap: 8,
          }}
        >
          <span style={{ font: "600 15px/1 var(--mono)", letterSpacing: "-.02em" }}>fastllm</span>
          <span style={{ font: "400 15px/1 var(--mono)", color: "var(--fg-4)" }}>proxy</span>
        </div>
        <nav
          style={{
            flex: 1,
            overflow: "auto",
            padding: "14px 10px",
            display: "flex",
            flexDirection: "column",
            gap: 16,
          }}
        >
          {visibleNav(config).map((group) => (
            <div key={group.label} style={{ display: "flex", flexDirection: "column", gap: 2 }}>
              <div
                style={{
                  font: "500 10px/1 var(--sans)",
                  color: "var(--fg-5)",
                  letterSpacing: ".12em",
                  padding: "0 8px 8px",
                }}
              >
                {group.label}
              </div>
              {group.items.map((item) => {
                const on = screen === item.id;
                return (
                  <button
                    key={item.id}
                    onClick={() => go(item.id)}
                    aria-current={on ? "page" : undefined}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 9,
                      width: "100%",
                      textAlign: "left",
                      background: on ? "var(--line-soft)" : "none",
                      border: "none",
                      borderRadius: 8,
                      color: on ? "var(--fg)" : "var(--fg-3)",
                      padding: "8px 9px",
                      font: `${on ? 500 : 400} 12.5px var(--sans)`,
                    }}
                  >
                    <span
                      style={{
                        width: 3,
                        height: 14,
                        borderRadius: 2,
                        flex: "none",
                        background: on ? "var(--accent)" : "transparent",
                      }}
                    />
                    {item.label}
                  </button>
                );
              })}
            </div>
          ))}
        </nav>
        <div
          style={{
            padding: 12,
            borderTop: "1px solid var(--line-mid)",
            display: "flex",
            alignItems: "center",
            gap: 10,
          }}
        >
          <div
            style={{
              width: 28,
              height: 28,
              borderRadius: 7,
              background: "var(--accent-bg)",
              color: "var(--accent-soft)",
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              font: "600 11px var(--mono)",
              flex: "none",
            }}
          >
            {(config?.you || who || "op").slice(0, 2).toUpperCase()}
          </div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div
              style={{
                font: "500 12px/1.2 var(--sans)",
                overflow: "hidden",
                textOverflow: "ellipsis",
                whiteSpace: "nowrap",
              }}
            >
              {/* `config.you` rather than only the login response: the session
                  cookie is HttpOnly, so after a reload the browser has no way
                  to know whose session it is holding, and this read the
                  operator's name until the first refresh and "signed in"
                  forever after. */}
              {config?.you || who || "signed in"}
            </div>
            <div style={{ font: "400 10px/1.3 var(--sans)", color: "var(--fg-4)" }}>
              session cookie
            </div>
          </div>
          <Button variant="small" onClick={logout} title="Log out">
            exit
          </Button>
        </div>
      </aside>

      <div style={{ flex: 1, display: "flex", flexDirection: "column", minWidth: 0 }}>
        <header
          style={{
            height: 56,
            flex: "none",
            borderBottom: "1px solid var(--line-mid)",
            display: "flex",
            alignItems: "center",
            gap: 14,
            padding: "0 24px",
            background: "var(--panel-2)",
          }}
        >
          <div style={{ font: "600 14px/1 var(--sans)", letterSpacing: "-.01em" }}>
            {active.title}
          </div>
          <div style={{ font: "400 12px/1 var(--sans)", color: "var(--fg-4)" }}>
            {active.subtitle}
          </div>
          <div style={{ flex: 1 }} />
          {config && (
            <>
              <Pill tone="quiet" mono>
                v{config.version}
              </Pill>
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: 8,
                  background: "var(--panel)",
                  border: "1px solid var(--line)",
                  borderRadius: 8,
                  padding: "7px 11px",
                  font: "400 12px var(--mono)",
                  color: "var(--fg-2)",
                }}
                title={
                  config.tls
                    ? "the admin API is served over TLS"
                    : "the admin API is served over plain HTTP — /snapshot carries upstream credentials"
                }
              >
                <Dot tone={config.tls ? "ok" : "warn"} size={6} />
                {`--role ${config.role}`}
                {!config.tls && " · no TLS"}
              </div>
            </>
          )}
        </header>

        <main style={{ flex: 1, overflow: "auto", padding: "24px 28px 48px" }}>
          {error && (
            <div style={{ marginBottom: 16 }}>
              <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>
            </div>
          )}
          <View onUnauthorised={onUnauthorised} config={config} go={go} />
        </main>
      </div>
    </div>
  );
}
