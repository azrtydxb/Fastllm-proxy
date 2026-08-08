import React, { useState } from "react";
import { api } from "../api.js";
import { attempt, useLoader } from "../load.js";
import {
  Button,
  Card,
  Empty,
  ErrorNote,
  Field,
  Grid,
  Loading,
  Mono,
  Muted,
  Pill,
  Row,
  Spacer,
  Stack,
  Table,
  Tr,
} from "../ui.jsx";

// Three tabs over the same RBAC model: who exists, what each role may do, and
// which models each role may invoke.
//
// The permission matrix writes through to `POST`/`DELETE
// /admin/roles/{name}/permissions`, and the verb list is closed on the server
// for a reason worth repeating here: a permission is only meaningful if
// something checks it. A matrix that accepted `model:delete` would show a
// granted cell that grants nothing, which is worse than not offering it.

const ADMIN_VERBS = ["usage:read", "key:create", "key:revoke", "config:write"];

const P_COLS = [
  { label: "PRINCIPAL", width: "1.3fr" },
  { label: "KIND", width: ".8fr" },
  { label: "ROLES", width: "1.8fr" },
  { label: "", width: "150px", align: "right" },
];

export function Principals({ onUnauthorised }) {
  const [tab, setTab] = useState("principals");
  const [draft, setDraft] = useState({ name: "", email: "", kind: "service_account" });
  const [roleDrafts, setRoleDrafts] = useState({});
  const [passwords, setPasswords] = useState({});

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [principals, roles, models] = await Promise.all([
        api.get("/admin/principals"),
        api.get("/admin/roles"),
        api.get("/admin/models"),
      ]);
      return { principals, roles, models };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data) return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const create = async (e) => {
    e.preventDefault();
    if (!draft.name.trim()) return;
    const ok = await attempt(
      () =>
        api.post("/admin/principals", {
          name: draft.name.trim(),
          kind: draft.kind,
          email: draft.email.trim() || undefined,
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setDraft({ name: "", email: "", kind: "service_account" });
      reload();
    }
  };

  const toggle = async (role, verb, resource, granted) => {
    const body = { verb, resource };
    const ok = await attempt(
      () =>
        granted
          ? api.del(`/admin/roles/${encodeURIComponent(role)}/permissions`, body)
          : api.post(`/admin/roles/${encodeURIComponent(role)}/permissions`, body),
      setError,
      onUnauthorised,
    );
    if (ok) reload();
  };

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Row gap={6} style={{ background: "var(--panel)", border: "1px solid var(--line)", borderRadius: 9, padding: 3, width: "fit-content" }}>
        {[
          ["principals", "Principals"],
          ["matrix", "Permission matrix"],
          ["grants", "Model grants"],
        ].map(([id, label]) => (
          <button
            key={id}
            onClick={() => setTab(id)}
            style={{
              background: tab === id ? "var(--accent-bg)" : "none",
              border: "none",
              borderRadius: 6,
              color: tab === id ? "var(--fg)" : "var(--fg-3)",
              padding: "6px 11px",
              font: "500 11.5px var(--sans)",
            }}
          >
            {label}
          </button>
        ))}
      </Row>

      {tab === "principals" && (
        <Grid cols="minmax(0,1fr) 300px">
          <Card>
            {data.principals.length === 0 ? (
              <Empty>No principals.</Empty>
            ) : (
              <Table cols={P_COLS}>
                {data.principals.map((p) => (
                  <Tr
                    key={p.id}
                    cols={P_COLS}
                    cells={[
                      <Row key="n" gap={9} style={{ flexWrap: "nowrap", minWidth: 0 }}>
                        <div
                          style={{
                            width: 26,
                            height: 26,
                            flex: "none",
                            borderRadius: 7,
                            background: "#1a1e24",
                            border: "1px solid var(--line)",
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "center",
                            font: "600 10px var(--mono)",
                            color: "var(--accent-soft)",
                          }}
                        >
                          {p.name.slice(0, 2).toUpperCase()}
                        </div>
                        <div style={{ minWidth: 0 }}>
                          <div style={{ font: "500 12px var(--sans)" }}>{p.name}</div>
                          {p.email && (
                            <div style={{ font: "400 10px var(--sans)", color: "var(--fg-5)" }}>
                              {p.email}
                            </div>
                          )}
                        </div>
                      </Row>,
                      <Mono key="k" style={{ font: "400 11px var(--mono)", color: "var(--fg-3)" }}>
                        {p.kind}
                      </Mono>,
                      <Row key="r" gap={5}>
                        {p.roles.map((r) => (
                          <span
                            key={r}
                            style={{ display: "inline-flex", alignItems: "center", gap: 4 }}
                          >
                            <Pill tone={r === "admin" ? "bad" : r === "inference" ? "ok" : "accent"}>
                              {r}
                            </Pill>
                            <button
                              title={`revoke ${r}`}
                              onClick={async () => {
                                const ok = await attempt(
                                  () => api.del(`/admin/principals/${p.id}/roles/${r}`),
                                  setError,
                                  onUnauthorised,
                                );
                                if (ok) reload();
                              }}
                              style={{
                                background: "none",
                                border: "none",
                                color: "var(--fg-5)",
                                padding: 0,
                                fontSize: 12,
                              }}
                            >
                              ×
                            </button>
                          </span>
                        ))}
                        <select
                          value={roleDrafts[p.id] || ""}
                          onChange={async (e) => {
                            const role = e.target.value;
                            setRoleDrafts({ ...roleDrafts, [p.id]: "" });
                            if (!role) return;
                            const ok = await attempt(
                              () => api.post(`/admin/principals/${p.id}/roles`, { role }),
                              setError,
                              onUnauthorised,
                            );
                            if (ok) reload();
                          }}
                          style={{ padding: "2px 6px", fontSize: 11, borderRadius: 999 }}
                        >
                          <option value="">+</option>
                          {data.roles
                            .filter((r) => !p.roles.includes(r.name))
                            .map((r) => (
                              <option key={r.name} value={r.name}>
                                {r.name}
                              </option>
                            ))}
                        </select>
                      </Row>,
                      <Row key="a" gap={6} style={{ justifyContent: "flex-end" }}>
                        {p.kind === "user" && (
                          <input
                            type="password"
                            placeholder="set password"
                            value={passwords[p.id] || ""}
                            onChange={(e) => setPasswords({ ...passwords, [p.id]: e.target.value })}
                            onKeyDown={async (e) => {
                              if (e.key !== "Enter" || !passwords[p.id]) return;
                              const ok = await attempt(
                                () =>
                                  api.put(`/admin/principals/${p.id}/password`, {
                                    password: passwords[p.id],
                                  }),
                                setError,
                                onUnauthorised,
                              );
                              if (ok) setPasswords({ ...passwords, [p.id]: "" });
                            }}
                            style={{ width: 96, fontSize: 11, padding: "4px 8px" }}
                          />
                        )}
                        <Button
                          variant="smallDanger"
                          onClick={async () => {
                            if (
                              !window.confirm(
                                `Delete ${p.name}? Its keys and role grants go with it.`,
                              )
                            )
                              return;
                            const ok = await attempt(
                              () => api.del(`/admin/principals/${p.id}`),
                              setError,
                              onUnauthorised,
                            );
                            if (ok) reload();
                          }}
                        >
                          delete
                        </Button>
                      </Row>,
                    ]}
                  />
                ))}
              </Table>
            )}
          </Card>

          <Stack gap={14}>
            <Card title="New principal">
              <form onSubmit={create}>
                <Stack gap={10}>
                  <Field label="NAME">
                    <input
                      value={draft.name}
                      onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                    />
                  </Field>
                  <Field label="EMAIL (OPTIONAL)">
                    <input
                      value={draft.email}
                      onChange={(e) => setDraft({ ...draft, email: e.target.value })}
                    />
                  </Field>
                  <Field
                    label="KIND"
                    hint="a user can log into this UI once a password is set; a service account cannot"
                  >
                    <select
                      value={draft.kind}
                      onChange={(e) => setDraft({ ...draft, kind: e.target.value })}
                    >
                      <option value="service_account">service_account</option>
                      <option value="user">user</option>
                    </select>
                  </Field>
                  <Button variant="primary" type="submit">
                    Create
                  </Button>
                </Stack>
              </form>
            </Card>
            <Card title="Roles">
              <Stack gap={8}>
                {data.roles.map((r) => (
                  <Row key={r.id} style={{ flexWrap: "nowrap" }}>
                    <Mono style={{ font: "500 12px var(--mono)" }}>{r.name}</Mono>
                    <Spacer />
                    <Muted>
                      {data.principals.filter((p) => p.roles.includes(r.name)).length} holder
                      {data.principals.filter((p) => p.roles.includes(r.name)).length === 1
                        ? ""
                        : "s"}
                    </Muted>
                  </Row>
                ))}
                <Muted>
                  Roles are seeded, not created here: each one exists because something checks it.
                </Muted>
              </Stack>
            </Card>
          </Stack>
        </Grid>
      )}

      {tab === "matrix" && (
        <Card
          title="Permission matrix"
          subtitle="the four admin permissions · click a cell to grant or revoke"
          right={
            <Row gap={10}>
              <Legend color="var(--accent-bg)" border="var(--accent-line)" label="granted" />
              <Legend color="#141922" border="var(--line)" label="denied" />
            </Row>
          }
        >
          <Matrix
            cols={ADMIN_VERBS}
            rows={data.roles.map((r) => ({
              key: r.name,
              label: r.name,
              sub: `${data.principals.filter((p) => p.roles.includes(r.name)).length} principals`,
              cells: ADMIN_VERBS.map((verb) => ({
                on: r.permissions.some((p) => p.verb === verb),
                onToggle: (on) => toggle(r.name, verb, "*", on),
              })),
            }))}
          />
          <div
            style={{
              marginTop: 16,
              paddingTop: 14,
              borderTop: "1px solid var(--line-mid)",
            }}
          >
            <Muted>
              There is no finer-grained permission than{" "}
              <Mono style={{ color: "var(--fg-2)" }}>config:write</Mono> for managing principals or
              virtual models — a permission per table would multiply roles for no operator-visible
              benefit. A session alone is not authority: every{" "}
              <Mono style={{ color: "var(--fg-2)" }}>/admin/*</Mono> handler checks a permission and
              answers 403 without one.
            </Muted>
          </div>
        </Card>
      )}

      {tab === "grants" && (
        <Card
          title={
            <>
              Per-model <Mono style={{ color: "var(--accent-soft)" }}>model:invoke</Mono> grants
            </>
          }
          subtitle="failover never widens reach — a candidate the caller lacks model:invoke on is dropped from the chain, including the deployment-wide fallback"
        >
          {data.models.length === 0 ? (
            <Empty>No models to grant.</Empty>
          ) : (
            <Matrix
              cols={data.models.map((m) => m.name)}
              rows={data.roles.map((r) => ({
                key: r.name,
                label: r.name,
                sub: r.permissions.some((p) => p.verb === "model:invoke" && p.resource === "*")
                  ? "all models"
                  : undefined,
                cells: data.models.map((m) => {
                  const all = r.permissions.some(
                    (p) => p.verb === "model:invoke" && p.resource === "*",
                  );
                  const one = r.permissions.some(
                    (p) => p.verb === "model:invoke" && p.resource === `model/${m.name}`,
                  );
                  return {
                    on: one,
                    // A wildcard grant already covers this model. Showing the
                    // cell as merely "off" would be a lie, and letting a click
                    // write a per-model row that changes nothing would be
                    // worse — it would read as a narrowing that did not happen.
                    implied: all && !one,
                    onToggle: (on) => toggle(r.name, "model:invoke", `model/${m.name}`, on),
                  };
                }),
              }))}
            />
          )}
        </Card>
      )}
    </Stack>
  );
}

function Legend({ color, border, label }) {
  return (
    <span
      style={{
        display: "flex",
        alignItems: "center",
        gap: 6,
        font: "400 11px var(--sans)",
        color: "var(--fg-3)",
      }}
    >
      <span
        style={{ width: 11, height: 11, borderRadius: 3, background: color, border: `1px solid ${border}` }}
      />
      {label}
    </span>
  );
}

function Matrix({ cols, rows }) {
  const template = `170px repeat(${cols.length},minmax(60px,1fr))`;
  return (
    <div style={{ overflowX: "auto" }}>
      <div style={{ minWidth: 120 + cols.length * 90 }}>
        <div
          style={{
            display: "grid",
            gridTemplateColumns: template,
            gap: 6,
            alignItems: "end",
            marginBottom: 8,
          }}
        >
          <span />
          {cols.map((c) => (
            <span
              key={c}
              style={{
                font: "500 11px/1.3 var(--mono)",
                color: "var(--fg-4)",
                textAlign: "center",
                wordBreak: "break-word",
              }}
            >
              {c}
            </span>
          ))}
        </div>
        {rows.map((row) => (
          <div
            key={row.key}
            style={{
              display: "grid",
              gridTemplateColumns: template,
              gap: 6,
              alignItems: "center",
              marginBottom: 6,
            }}
          >
            <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
              <Mono style={{ font: "500 12px var(--mono)" }}>{row.label}</Mono>
              {row.sub && (
                <span style={{ font: "400 10px var(--sans)", color: "var(--fg-5)" }}>{row.sub}</span>
              )}
            </div>
            {row.cells.map((cell, i) => (
              <button
                key={i}
                onClick={() => cell.onToggle(cell.on)}
                title={
                  cell.implied
                    ? "already granted by a wildcard — revoke that to narrow it"
                    : cell.on
                      ? "granted — click to revoke"
                      : "denied — click to grant"
                }
                style={{
                  height: 34,
                  background: cell.on ? "#1d3a63" : cell.implied ? "var(--warn-bg)" : "#141922",
                  border: `1px solid ${cell.on ? "var(--accent-line)" : cell.implied ? "var(--warn-line)" : "var(--line)"}`,
                  borderRadius: 7,
                  color: cell.on
                    ? "var(--accent-soft)"
                    : cell.implied
                      ? "var(--warn-fg)"
                      : "#3f4650",
                  font: "500 12px var(--mono)",
                }}
              >
                {cell.on ? "✓" : cell.implied ? "*" : "·"}
              </button>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}
