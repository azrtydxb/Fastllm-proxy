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
  Stack,
  Table,
  Tr,
} from "../ui.jsx";

// A2A agents, managed exactly as MCP servers are, because they are the same
// shape: an address, a credential, and the two auth knobs.
//
// The credential field writes and never reads — the API reports whether one is
// set and never what it is.
//
// The column that does not exist on the MCP screen is `protocol_version`, and
// it is pinned rather than inferred: 0.3 and 1.0 are different wire formats,
// and a gateway that guesses from the request produces an agent card saying
// one thing and responses that are the other.

const COLS = [
  { label: "NAME", width: "1fr" },
  { label: "URL", width: "2fr" },
  { label: "A2A", width: ".6fr" },
  { label: "CREDENTIAL", width: "1fr" },
  { label: "STATE", width: ".8fr" },
  { label: "", width: "150px", align: "right" },
];

const EMPTY = {
  name: "",
  url: "",
  protocol_version: "0.3",
  description: "",
  auth_header: "authorization",
  auth_scheme: "Bearer",
  upstream_api_key: "",
};

export function Agents({ onUnauthorised, go }) {
  const [draft, setDraft] = useState(EMPTY);

  const { data, error, loading, reload, setError } = useLoader(
    () => api.get("/admin/a2a-agents"),
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data) return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const servers = data.data || [];

  const create = async (e) => {
    e.preventDefault();
    if (!draft.name.trim() || !draft.url.trim()) return;
    const ok = await attempt(
      () =>
        api.post("/admin/a2a-agents", {
          name: draft.name.trim(),
          url: draft.url.trim(),
          protocol_version: draft.protocol_version,
          description: draft.description.trim(),
          auth_header: draft.auth_header.trim() || "authorization",
          // Sent as written: "" is meaningful — it means send the credential
          // with no prefix, which is what several MCP hosts want.
          auth_scheme: draft.auth_scheme,
          upstream_api_key: draft.upstream_api_key,
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setDraft(EMPTY);
      reload();
    }
  };

  const toggle = async (s) => {
    const ok = await attempt(
      () => api.patch(`/admin/a2a-agents/${s.id}`, { enabled: !s.enabled }),
      setError,
      onUnauthorised,
    );
    if (ok) reload();
  };

  const remove = async (s) => {
    if (!window.confirm(`Delete agent ${s.name}? It stops being reachable through this gateway.`)) return;
    const ok = await attempt(
      () => api.del(`/admin/a2a-agents/${s.id}`),
      setError,
      onUnauthorised,
    );
    if (ok) reload();
  };

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Row>
        <Muted>
          One address in front of every agent. A card served through here is
          rewritten to point at this gateway, so the client&apos;s next call is
          still authorised and still attributed — served unchanged, that URL
          would send it straight to the agent.
        </Muted>
      </Row>

      <Grid cols="minmax(0,1fr) 360px">
        <Stack gap={14}>
          <Card>
            {servers.length === 0 ? (
              <Empty>
                No agents. Add one and any key granted{" "}
                <Mono>agent:invoke</Mono> can reach it at{" "}
                <Mono>/v1/agents/&lt;name&gt;</Mono>.
              </Empty>
            ) : (
              <Table cols={COLS}>
                {servers.map((s) => (
                  <Tr
                    key={s.id}
                    cols={COLS}
                    cells={[
                      <Mono key="n" style={{ font: "500 12px var(--mono)" }}>
                        {s.name}
                      </Mono>,
                      <Mono key="u" style={{ color: "var(--fg-3)", fontSize: 11 }}>
                        {s.url}
                      </Mono>,
                      <Pill key="t" tone={s.protocol_version === "1.0" ? "violet" : "ok"}>
                        {s.protocol_version}
                      </Pill>,
                      <span key="c" style={{ color: "var(--fg-3)" }}>
                        {s.credential_set ? (
                          <>
                            credential set{" "}
                            <Mono style={{ fontSize: 11, color: "var(--fg-4)" }}>
                              {s.auth_header}
                              {s.auth_scheme ? ` ${s.auth_scheme} …` : " …"}
                            </Mono>
                          </>
                        ) : (
                          "no credential"
                        )}
                      </span>,
                      <Pill key="e" tone={s.enabled ? "ok" : "warn"}>
                        {s.enabled ? "enabled" : "disabled"}
                      </Pill>,
                      <Row key="a" gap={6} style={{ justifyContent: "flex-end" }}>
                        <Button variant="small" onClick={() => toggle(s)}>
                          {s.enabled ? "disable" : "enable"}
                        </Button>
                        <Button variant="smallDanger" onClick={() => remove(s)}>
                          delete
                        </Button>
                      </Row>,
                    ]}
                  />
                ))}
              </Table>
            )}
          </Card>

          {servers.length > 0 && (
            <Card title="Who can reach these" tone="warn">
              <Muted>
                An agent existing is not a key being able to run it. Access is{" "}
                <Mono>agent:invoke</Mono> on <Mono>agent/&lt;name&gt;</Mono>, granted
                on{" "}
                <Button variant="small" onClick={() => go && go("rbac")}>
                  Principals &amp; roles
                </Button>
                . A new agent is reachable by nobody until someone says
                otherwise, and neither <Mono>model:invoke</Mono> nor{" "}
                <Mono>mcp:invoke</Mono> implies it — an agent acts.
              </Muted>
            </Card>
          )}
        </Stack>

        <Stack gap={14}>
          <Card title="Add an agent">
            <form onSubmit={create}>
              <Stack gap={10}>
                <Field label="NAME" hint="also the namespace its tools appear under">
                  <input
                    placeholder="planner"
                    value={draft.name}
                    onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                  />
                </Field>
                <Field label="URL">
                  <input
                    placeholder="https://agent.example.com/a2a"
                    value={draft.url}
                    onChange={(e) => setDraft({ ...draft, url: e.target.value })}
                  />
                </Field>
                <Field
                  label="PROTOCOL VERSION"
                  hint="pinned, not inferred — the card and the responses have to agree"
                >
                  <select
                    value={draft.protocol_version}
                    onChange={(e) => setDraft({ ...draft, protocol_version: e.target.value })}
                  >
                    <option value="0.3">0.3 · kind-discriminated</option>
                    <option value="1.0">1.0 · protobuf JSON</option>
                  </select>
                </Field>
                <Field label="DESCRIPTION">
                  <input
                    value={draft.description}
                    onChange={(e) => setDraft({ ...draft, description: e.target.value })}
                  />
                </Field>
                <Field
                  label="CREDENTIAL"
                  hint="encrypted before it reaches the database, and never readable back"
                >
                  <input
                    type="password"
                    placeholder="optional"
                    value={draft.upstream_api_key}
                    onChange={(e) => setDraft({ ...draft, upstream_api_key: e.target.value })}
                  />
                </Field>
                <Grid cols="1fr 1fr" gap={10}>
                  <Field label="AUTH HEADER">
                    <input
                      value={draft.auth_header}
                      onChange={(e) => setDraft({ ...draft, auth_header: e.target.value })}
                    />
                  </Field>
                  <Field label="SCHEME" hint="empty sends the key raw">
                    <input
                      value={draft.auth_scheme}
                      onChange={(e) => setDraft({ ...draft, auth_scheme: e.target.value })}
                    />
                  </Field>
                </Grid>
                <Button variant="primary" type="submit">
                  Add
                </Button>
              </Stack>
            </form>
          </Card>

          <Card title="How a client uses this">
            <Stack gap={8}>
              <Muted>
                One address, one key. The tools of every server this key may
                reach come back from a single call:
              </Muted>
              <Mono style={{ fontSize: 11, color: "var(--fg-3)", lineHeight: 1.6 }}>
                GET /v1/agents
                <br />
                GET /v1/agents/&lt;name&gt;/.well-known/agent-card.json
                <br />
                POST /v1/agents/&lt;name&gt;
              </Mono>
              <Muted>
                Only the JSON-RPC methods this gateway forwards are accepted.
                An unknown one is refused rather than passed through blind, on
                a credential the caller never sees.
              </Muted>
            </Stack>
          </Card>
        </Stack>
      </Grid>
    </Stack>
  );
}
