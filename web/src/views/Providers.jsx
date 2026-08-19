import React, { useState } from "react";
import { api } from "../api.js";
import { useLoader } from "../load.js";
import { mergeBackends, backendKey } from "../fleet.js";
import {
  Chip,
  Card,
  Dot,
  Ellipsis,
  Empty,
  ErrorNote,
  Grid,
  Loading,
  Mono,
  Muted,
  Pill,
  Row,
  Stack,
  fmtInt,
  hostOf,
} from "../ui.jsx";

// A provider is a grouping this screen invents, not a thing the API models.
//
// The database has models and backends; "OpenRouter" is what a human calls
// every backend whose api_base points at openrouter.ai. Grouping them here is
// useful — it answers "what are we actually talking to, and with what
// credentials" in one look — but it is presentation, so nothing is written
// back at this level and nothing claims to be a provider record.
//
// What is deliberately absent: region and latency. Neither is modelled
// anywhere. Region would have to be parsed out of a hostname for the two
// clouds that encode it and guessed for the rest, and latency is per process
// on each proxy's own histogram. Inventing either would put a number on this
// page that no query could reproduce.

const FILTERS = ["All", "Hosted", "Self-hosted", "Native protocol"];

export function Providers({ onUnauthorised, go }) {
  const [filter, setFilter] = useState("All");
  const [search, setSearch] = useState("");

  const { data, error, loading, setError } = useLoader(
    async () => {
      const [models, fleet] = await Promise.all([
        api.get("/admin/models"),
        api.get("/admin/fleet"),
      ]);
      return { models, fleet };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const health = new Map(mergeBackends(data.fleet).map((b) => [b.key, b]));

  // Group by origin, which is what distinguishes one provider from another —
  // path segments are per-model, and splitting on them would show four
  // "providers" for one Vertex project.
  const groups = new Map();
  for (const m of data.models) {
    for (const b of m.backends) {
      const origin = originOf(b.api_base);
      let g = groups.get(origin);
      if (!g) {
        g = {
          origin,
          host: hostOf(b.api_base),
          bases: new Set(),
          models: new Set(),
          protocols: new Set(),
          credentialled: 0,
          backends: 0,
          up: 0,
          reported: 0,
        };
        groups.set(origin, g);
      }
      g.bases.add(b.api_base);
      g.models.add(m.name);
      g.protocols.add(b.protocol);
      g.backends += 1;
      if (b.has_upstream_api_key) g.credentialled += 1;
      const h = health.get(backendKey(b.api_base, b.upstream_model || m.name));
      if (h) {
        g.reported += 1;
        if (h.healthy) g.up += 1;
      }
    }
  }

  const all = [...groups.values()].sort((a, b) => a.host.localeCompare(b.host));
  const shown = all
    .filter((g) => {
      if (filter === "Hosted") return g.credentialled > 0;
      if (filter === "Self-hosted") return g.credentialled === 0;
      if (filter === "Native protocol")
        return [...g.protocols].some((p) => p !== "openai");
      return true;
    })
    .filter(
      (g) => !search || g.host.toLowerCase().includes(search.toLowerCase()),
    );

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Row gap={10}>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 8,
            background: "var(--panel)",
            border: "1px solid var(--line)",
            borderRadius: 8,
            padding: "8px 12px",
            width: 300,
          }}
        >
          <Mono style={{ color: "var(--fg-5)", fontSize: 12 }}>/</Mono>
          <input
            placeholder="filter by host"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            style={{
              background: "none",
              border: "none",
              padding: 0,
              width: "100%",
            }}
          />
        </div>
        {FILTERS.map((f) => (
          <Chip key={f} active={filter === f} onClick={() => setFilter(f)}>
            {f}
          </Chip>
        ))}
      </Row>

      <Muted>
        A provider is a row in a table, not a code change — anything speaking
        the OpenAI API is already supported. Backends are added on the{" "}
        <a href="#/models" onClick={() => go("models")}>
          Models
        </a>{" "}
        screen, where they belong to the model they serve.
      </Muted>

      {shown.length === 0 ? (
        <Card>
          <Empty>
            {all.length === 0
              ? "No backends are configured yet."
              : "No provider matches that filter."}
          </Empty>
        </Card>
      ) : (
        <Grid cols={3}>
          {shown.map((g) => {
            const native = [...g.protocols].filter((p) => p !== "openai");
            return (
              <Card key={g.origin}>
                <Stack gap={12}>
                  <Row style={{ flexWrap: "nowrap", alignItems: "flex-start" }}>
                    <Row gap={10} style={{ flexWrap: "nowrap", minWidth: 0 }}>
                      <div
                        style={{
                          width: 30,
                          height: 30,
                          flex: "none",
                          borderRadius: 8,
                          background: "#1a1e24",
                          border: "1px solid var(--line)",
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "center",
                          font: "600 11px var(--mono)",
                          color: "var(--accent-soft)",
                        }}
                      >
                        {g.host
                          .replace(/^www\./, "")
                          .slice(0, 2)
                          .toUpperCase()}
                      </div>
                      <div style={{ minWidth: 0 }}>
                        <Ellipsis
                          style={{ font: "600 13px/1.2 var(--sans)" }}
                          title={g.host}
                        >
                          {g.host}
                        </Ellipsis>
                        <div
                          style={{
                            font: "400 10px/1.4 var(--sans)",
                            color: "var(--fg-4)",
                          }}
                        >
                          {fmtInt(g.models.size)} model
                          {g.models.size === 1 ? "" : "s"} ·{" "}
                          {fmtInt(g.backends)} backend
                          {g.backends === 1 ? "" : "s"}
                        </div>
                      </div>
                    </Row>
                    <div style={{ flex: 1 }} />
                    <Pill tone={native.length ? "violet" : "neutral"} mono>
                      {native.length ? native.join(" · ") : "openai"}
                    </Pill>
                  </Row>

                  {[...g.bases].slice(0, 2).map((base) => (
                    <Ellipsis
                      key={base}
                      title={base}
                      style={{
                        font: "400 11px/1.4 var(--mono)",
                        color: "var(--fg-3)",
                        background: "var(--panel-2)",
                        border: "1px solid var(--line-mid)",
                        borderRadius: 7,
                        padding: "8px 10px",
                      }}
                    >
                      {base}
                    </Ellipsis>
                  ))}
                  {g.bases.size > 2 && (
                    <Muted>and {g.bases.size - 2} more paths</Muted>
                  )}

                  <Row
                    style={{
                      paddingTop: 10,
                      borderTop: "1px solid var(--line-mid)",
                      flexWrap: "nowrap",
                    }}
                  >
                    <Row gap={6} style={{ flexWrap: "nowrap" }}>
                      <Dot tone={g.credentialled > 0 ? "ok" : "muted"} />
                      <Muted>
                        {g.credentialled === 0
                          ? "no credential"
                          : g.credentialled === g.backends
                            ? "credential set"
                            : `${g.credentialled} of ${g.backends} credentialled`}
                      </Muted>
                    </Row>
                    <div style={{ flex: 1 }} />
                    {g.reported === 0 ? (
                      <Muted>not yet probed</Muted>
                    ) : (
                      <Pill
                        tone={
                          g.up === g.reported
                            ? "ok"
                            : g.up === 0
                              ? "bad"
                              : "warn"
                        }
                      >
                        {g.up} of {g.reported} up
                      </Pill>
                    )}
                  </Row>
                </Stack>
              </Card>
            );
          })}
        </Grid>
      )}
    </Stack>
  );
}

function originOf(apiBase) {
  try {
    return new URL(apiBase).origin;
  } catch {
    return apiBase;
  }
}
