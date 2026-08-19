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

// MCP servers, managed the way models are — because they are the same shape.
//
// What this screen deliberately does not do is show a credential. The API
// reports whether one is set and never what it is, so there is nothing here to
// reveal; the field below writes and never reads.
//
// The other thing worth knowing while looking at this screen is that a server
// existing is not a key being able to reach it. Grants are `mcp:invoke` on
// `mcp/<name>` and live on Principals & roles, so a newly added server is
// reachable by nobody until someone says otherwise — which is the safe
// direction, and confusing without the note this screen carries.

const COLS = [
  { label: "NAME", width: "1fr" },
  { label: "URL", width: "2fr" },
  { label: "TRANSPORT", width: ".8fr" },
  { label: "CREDENTIAL", width: "1fr" },
  { label: "STATE", width: ".8fr" },
  { label: "", width: "150px", align: "right" },
];

const EMPTY = {
  name: "",
  url: "",
  transport: "http",
  description: "",
  auth_header: "authorization",
  auth_scheme: "Bearer",
  upstream_api_key: "",
};

export function McpServers({ onUnauthorised, go }) {
  const [draft, setDraft] = useState(EMPTY);

  const { data, error, loading, reload, setError } = useLoader(
    () => api.get("/admin/mcp-servers"),
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const servers = data.data || [];

  const create = async (e) => {
    e.preventDefault();
    if (!draft.name.trim() || !draft.url.trim()) return;
    const ok = await attempt(
      () =>
        api.post("/admin/mcp-servers", {
          name: draft.name.trim(),
          url: draft.url.trim(),
          transport: draft.transport,
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
      () => api.patch(`/admin/mcp-servers/${s.id}`, { enabled: !s.enabled }),
      setError,
      onUnauthorised,
    );
    if (ok) reload();
  };

  const remove = async (s) => {
    if (
      !window.confirm(
        `Delete MCP server ${s.name}? Its tools stop being reachable.`,
      )
    )
      return;
    const ok = await attempt(
      () => api.del(`/admin/mcp-servers/${s.id}`),
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
          One endpoint in front of every tool server. Tools are namespaced{" "}
          <Mono>server__tool</Mono>, so two servers can both expose{" "}
          <Mono>search</Mono> without the gateway losing track of which one a
          model meant.
        </Muted>
      </Row>

      <Grid cols="minmax(0,1fr) 360px">
        <Stack gap={14}>
          <Card>
            {servers.length === 0 ? (
              <Empty>
                No MCP servers. Add one and any key granted{" "}
                <Mono>mcp:invoke</Mono> can call its tools through{" "}
                <Mono>/v1/mcp/tools/call</Mono>.
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
                      <Mono
                        key="u"
                        style={{ color: "var(--fg-3)", fontSize: 11 }}
                      >
                        {s.url}
                      </Mono>,
                      <Pill
                        key="t"
                        tone={s.transport === "http" ? "ok" : "violet"}
                      >
                        {s.transport}
                      </Pill>,
                      <span key="c" style={{ color: "var(--fg-3)" }}>
                        {s.credential_set ? (
                          <>
                            credential set{" "}
                            <Mono
                              style={{ fontSize: 11, color: "var(--fg-4)" }}
                            >
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
                      <Row
                        key="a"
                        gap={6}
                        style={{ justifyContent: "flex-end" }}
                      >
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
                A server existing is not a key being able to call it. Access is{" "}
                <Mono>mcp:invoke</Mono> on <Mono>mcp/&lt;name&gt;</Mono>,
                granted on{" "}
                <Button variant="small" onClick={() => go && go("rbac")}>
                  Principals &amp; roles
                </Button>
                . A new server is reachable by nobody until someone says
                otherwise, and <Mono>model:invoke</Mono> does not imply it —
                tools have side effects and models do not.
              </Muted>
            </Card>
          )}
        </Stack>

        <Stack gap={14}>
          <Card title="Add a server">
            <form onSubmit={create}>
              <Stack gap={10}>
                <Field
                  label="NAME"
                  hint="also the namespace its tools appear under"
                >
                  <input
                    placeholder="github"
                    value={draft.name}
                    onChange={(e) =>
                      setDraft({ ...draft, name: e.target.value })
                    }
                  />
                </Field>
                <Field label="URL">
                  <input
                    placeholder="https://mcp.example.com/mcp"
                    value={draft.url}
                    onChange={(e) =>
                      setDraft({ ...draft, url: e.target.value })
                    }
                  />
                </Field>
                <Field
                  label="TRANSPORT"
                  hint="streamable HTTP, or the older SSE framing"
                >
                  <select
                    value={draft.transport}
                    onChange={(e) =>
                      setDraft({ ...draft, transport: e.target.value })
                    }
                  >
                    <option value="http">http · streamable</option>
                    <option value="sse">sse</option>
                  </select>
                </Field>
                <Field label="DESCRIPTION">
                  <input
                    value={draft.description}
                    onChange={(e) =>
                      setDraft({ ...draft, description: e.target.value })
                    }
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
                    onChange={(e) =>
                      setDraft({ ...draft, upstream_api_key: e.target.value })
                    }
                  />
                </Field>
                <Grid cols="1fr 1fr" gap={10}>
                  <Field label="AUTH HEADER">
                    <input
                      value={draft.auth_header}
                      onChange={(e) =>
                        setDraft({ ...draft, auth_header: e.target.value })
                      }
                    />
                  </Field>
                  <Field label="SCHEME" hint="empty sends the key raw">
                    <input
                      value={draft.auth_scheme}
                      onChange={(e) =>
                        setDraft({ ...draft, auth_scheme: e.target.value })
                      }
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
              <Mono
                style={{ fontSize: 11, color: "var(--fg-3)", lineHeight: 1.6 }}
              >
                POST /v1/mcp/tools/list
                <br />
                POST /v1/mcp/tools/call
                <br />
                {"{"}"name": "github__search", "arguments": {"{}"}
                {"}"}
              </Mono>
              <Muted>
                A server that does not answer is named in{" "}
                <Mono>unreachable</Mono> rather than failing the whole list, so
                one server being down does not hide the other three.
              </Muted>
            </Stack>
          </Card>
        </Stack>
      </Grid>
    </Stack>
  );
}
