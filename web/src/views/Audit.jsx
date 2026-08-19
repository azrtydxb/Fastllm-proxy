import React, { useState } from "react";
import { api, query } from "../api.js";
import { useLoader } from "../load.js";
import {
  Button,
  Card,
  Chip,
  Dot,
  Ellipsis,
  Empty,
  ErrorNote,
  Grid,
  Loading,
  Mono,
  Muted,
  Row,
  Spacer,
  Stack,
  Table,
  Tr,
} from "../ui.jsx";

// The change log, newest first.
//
// Paginated on `before` — the id of the oldest row already held — rather than
// an offset. On an append-only log new rows arrive at the head while you page,
// so an offset skips or repeats rows, guaranteed rather than occasionally.

const COLS = [
  { label: "AT", width: "90px" },
  { label: "ACTOR", width: "1.1fr" },
  { label: "ACTION", width: ".8fr" },
  { label: "TARGET", width: "1.6fr" },
  { label: "DETAIL", width: "1.2fr" },
];

const TARGET_FILTERS = [
  ["", "All"],
  ["/admin/keys", "keys"],
  ["/admin/models", "models"],
  ["/admin/principals", "principals"],
  ["/admin/roles", "roles"],
  ["/admin/virtual-models", "routing"],
];

export function Audit({ onUnauthorised }) {
  const [target, setTarget] = useState("");
  const [search, setSearch] = useState("");
  const [pages, setPages] = useState([]);

  const before = pages.length ? pages[pages.length - 1] : undefined;

  const { data, error, loading, setError } = useLoader(
    () =>
      api.get(
        "/admin/audit" +
          query({ limit: 50, target: target || undefined, before }),
      ),
    { onUnauthorised, deps: [target, before] },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const rows = data.filter(
    (r) =>
      !search ||
      r.actor_name.toLowerCase().includes(search.toLowerCase()) ||
      r.target.toLowerCase().includes(search.toLowerCase()),
  );

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Row gap={8}>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 8,
            background: "var(--panel)",
            border: "1px solid var(--line)",
            borderRadius: 8,
            padding: "8px 12px",
            width: 280,
          }}
        >
          <Mono style={{ color: "var(--fg-5)", fontSize: 12 }}>/</Mono>
          <input
            placeholder="actor or target"
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
        {TARGET_FILTERS.map(([v, l]) => (
          <Chip
            key={l}
            active={target === v}
            onClick={() => {
              setTarget(v);
              setPages([]);
            }}
          >
            {l}
          </Chip>
        ))}
        <Spacer />
        <Muted>Recorded by a layer over every route, not per handler</Muted>
      </Row>

      <Card>
        {rows.length === 0 ? (
          <Empty>
            {search || target
              ? "Nothing matches that filter on this page."
              : "No changes recorded yet."}
          </Empty>
        ) : (
          <Table cols={COLS}>
            {rows.map((r) => (
              <Tr
                key={r.id}
                cols={COLS}
                cells={[
                  <Mono key="t" style={{ color: "var(--fg-4)", fontSize: 11 }}>
                    {new Date(r.at).toLocaleString([], {
                      month: "short",
                      day: "2-digit",
                      hour: "2-digit",
                      minute: "2-digit",
                    })}
                  </Mono>,
                  <Row
                    key="a"
                    gap={8}
                    style={{ flexWrap: "nowrap", minWidth: 0 }}
                  >
                    <Dot tone={r.actor_id === null ? "muted" : "accent"} />
                    <Ellipsis
                      style={{ font: "400 12px var(--mono)" }}
                      title={r.actor_name}
                    >
                      {r.actor_name}
                    </Ellipsis>
                  </Row>,
                  <Mono
                    key="v"
                    style={{
                      font: "500 11px var(--mono)",
                      color:
                        r.action === "DELETE"
                          ? "var(--bad-fg)"
                          : r.action === "POST"
                            ? "var(--ok-fg)"
                            : "var(--fg-2)",
                    }}
                  >
                    {r.action}
                  </Mono>,
                  <Ellipsis
                    key="g"
                    style={{
                      font: "400 12px var(--mono)",
                      color: "var(--fg-2)",
                    }}
                    title={r.target}
                  >
                    {r.target}
                  </Ellipsis>,
                  <Mono key="d" style={{ fontSize: 11, color: "var(--fg-4)" }}>
                    {r.detail && r.detail.status ? `→ ${r.detail.status}` : "—"}
                  </Mono>,
                ]}
              />
            ))}
          </Table>
        )}
        <Row gap={8} style={{ marginTop: 12 }}>
          {pages.length > 0 && (
            <Button
              variant="small"
              onClick={() => setPages(pages.slice(0, -1))}
            >
              ← newer
            </Button>
          )}
          <Spacer />
          {data.length === 50 && (
            <Button
              variant="small"
              onClick={() => setPages([...pages, data[data.length - 1].id])}
            >
              older →
            </Button>
          )}
        </Row>
      </Card>

      <Grid cols={3}>
        <Card title="Reads are not recorded">
          <Muted>
            Auditing every list call would bury the changes in noise. The same
            goes for the handful of routes that are POST only because they take
            a body — a dry-run changes nothing, and recording it would dilute a
            log whose value is that every row is a change.
          </Muted>
        </Card>
        <Card title="A 403 is an attempt, not a change">
          <Muted>
            Rejected calls are kept out of the trail so it cannot lie in the
            direction that matters most: everything here actually happened.
          </Muted>
        </Card>
        <Card title="Coarse on purpose">
          <Muted>
            A row says a principal&rsquo;s roles were changed and by whom, never
            which role — and never the request body, which carries passwords and
            upstream credentials. Complete and coarse beats detailed and full of
            holes; the application log carries the rest.
          </Muted>
        </Card>
      </Grid>
    </Stack>
  );
}
