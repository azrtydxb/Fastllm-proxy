import React, { useState } from "react";
import { api, query } from "../api.js";
import { useLoader } from "../load.js";
import {
  Bar,
  Card,
  Chip,
  Empty,
  ErrorNote,
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
  fmtCompact,
  fmtInt,
  fmtMoney,
} from "../ui.jsx";

// Consumption, aggregated three ways over one window.
//
// `unpriced_requests` is on every row and in every total, and that is the
// point of this screen rather than a footnote: a request whose model has no
// price stores NULL, not zero. Summing NULL as zero would report a
// self-hosted-looking spend for traffic that simply has no price attached, and
// the difference between "this was free" and "we do not know what this cost"
// is exactly what a cost review turns on.

const RANGES = [
  ["1", "24h"],
  ["7", "7d"],
  ["30", "30d"],
  ["", "all time"],
];

const GROUPS = [
  ["principal", "By principal"],
  ["model", "By model"],
  ["virtual_model", "By frontend model"],
  ["day", "By day"],
];

export function Usage({ onUnauthorised }) {
  const [days, setDays] = useState("30");
  const [group, setGroup] = useState("principal");

  const since = days
    ? new Date(Date.now() - Number(days) * 86400_000).toISOString()
    : undefined;

  const { data, error, loading, setError } = useLoader(
    async () => {
      const [rows, byModel] = await Promise.all([
        api.get("/admin/usage" + query({ group_by: group, since, limit: 100 })),
        api.get("/admin/usage" + query({ group_by: "model", since, limit: 12 })),
      ]);
      return { rows, byModel };
    },
    { onUnauthorised, deps: [group, days] },
  );

  if (loading && !data) return <Loading />;
  if (!data) return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const rows = data.rows || [];
  const totals = rows.reduce(
    (a, r) => ({
      requests: a.requests + r.requests,
      prompt: a.prompt + r.prompt_tokens,
      completion: a.completion + r.completion_tokens,
      cost: a.cost + r.cost_micros,
      unpriced: a.unpriced + r.unpriced_requests,
    }),
    { requests: 0, prompt: 0, completion: 0, cost: 0, unpriced: 0 },
  );
  const tokens = totals.prompt + totals.completion;
  // Not "some are unpriced" but "none are priced": the difference between a
  // total that is missing a part and a total that does not exist.
  const allUnpriced = totals.requests > 0 && totals.unpriced === totals.requests;
  const maxCost = Math.max(1, ...(data.byModel || []).map((r) => r.cost_micros));

  const cols = [
    { label: GROUPS.find(([g]) => g === group)[1].replace("By ", "").toUpperCase(), width: "1.3fr" },
    { label: "REQUESTS", width: ".8fr", align: "right" },
    { label: "PROMPT", width: ".9fr", align: "right" },
    { label: "COMPLETION", width: ".9fr", align: "right" },
    { label: "SPEND", width: "1fr", align: "right" },
  ];

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Row gap={8}>
        {RANGES.map(([v, l]) => (
          <Chip key={l} active={days === v} onClick={() => setDays(v)}>
            {l}
          </Chip>
        ))}
        <Spacer />
        {GROUPS.map(([v, l]) => (
          <Chip key={v} active={group === v} onClick={() => setGroup(v)}>
            {l}
          </Chip>
        ))}
      </Row>

      <Grid cols={4}>
        <Kpi label="REQUESTS" value={fmtCompact(totals.requests)} />
        <Kpi label="TOKENS" value={fmtCompact(tokens)} sub={`${fmtCompact(totals.prompt)} prompt`} />
        {/* When every request is unpriced there is no spend figure to give,
            and "$0.00" would answer "we spent nothing" to a question whose
            real answer is "we do not know". Only a mix has a total worth
            printing. */}
        <Kpi
          label="SPEND"
          value={allUnpriced ? "—" : fmtMoney(totals.cost)}
          sub={
            allUnpriced
              ? `no priced traffic — all ${fmtInt(totals.requests)} requests are unpriced`
              : totals.unpriced > 0
                ? `${fmtInt(totals.unpriced)} unpriced request${totals.unpriced === 1 ? "" : "s"} not counted`
                : "every request priced"
          }
          tone={totals.unpriced > 0 ? "warn" : undefined}
        />
        <Kpi
          label="COST / 1M TOKENS"
          value={allUnpriced || tokens === 0 ? "—" : fmtMoney((totals.cost / tokens) * 1e6)}
          sub={allUnpriced ? "needs at least one priced model" : "over priced traffic only"}
          tone={allUnpriced ? "warn" : undefined}
        />
      </Grid>

      <Grid cols="1.4fr minmax(0,1fr)">
        <Card
          title={GROUPS.find(([g]) => g === group)[1]}
          right={<Muted>{days ? `last ${days} day${days === "1" ? "" : "s"}` : "all recorded usage"}</Muted>}
        >
          {rows.length === 0 ? (
            <Empty>
              No usage recorded for this window. Rows exist only for principals whose consumption is
              tracked — those with a budget or a token rate limit.
            </Empty>
          ) : (
            <Table cols={cols}>
              {rows.map((r, i) => (
                <Tr
                  key={(r.key || "—") + i}
                  cols={cols}
                  cells={[
                    <Mono key="k" style={{ color: "var(--fg)" }}>
                      {r.key || "—"}
                    </Mono>,
                    <Mono key="r" style={{ color: "var(--fg-2)" }}>
                      {fmtCompact(r.requests)}
                    </Mono>,
                    <Mono key="p" style={{ color: "var(--fg-2)" }}>
                      {fmtCompact(r.prompt_tokens)}
                    </Mono>,
                    <Mono key="c" style={{ color: "var(--fg-2)" }}>
                      {fmtCompact(r.completion_tokens)}
                    </Mono>,
                    r.unpriced_requests === r.requests ? (
                      <Pill key="s" tone="warn" style={{ marginLeft: "auto" }}>
                        unpriced
                      </Pill>
                    ) : (
                      <Mono key="s" style={{ color: "var(--fg)" }}>
                        {fmtMoney(r.cost_micros)}
                        {r.unpriced_requests > 0 && (
                          <span style={{ color: "var(--warn-fg)" }} title={`${r.unpriced_requests} of these requests had no price and contribute nothing to this figure`}>
                            {" "}
                            +{fmtInt(r.unpriced_requests)}?
                          </span>
                        )}
                      </Mono>
                    ),
                  ]}
                />
              ))}
            </Table>
          )}
        </Card>

        <Stack gap={14}>
          <Card title="Spend by model">
            {(data.byModel || []).length === 0 ? (
              <Empty>Nothing recorded.</Empty>
            ) : (
              (data.byModel || []).map((r, i) => (
                <div
                  key={(r.key || "—") + i}
                  style={{ display: "flex", flexDirection: "column", gap: 6, marginBottom: 14 }}
                >
                  <Row style={{ flexWrap: "nowrap" }}>
                    <Mono style={{ fontSize: 12 }}>{r.key || "—"}</Mono>
                    <Spacer />
                    <Muted>
                      {r.unpriced_requests === r.requests ? "unpriced" : fmtMoney(r.cost_micros)}
                    </Muted>
                  </Row>
                  <Bar
                    pct={(r.cost_micros / maxCost) * 100}
                    tone={
                      r.unpriced_requests === r.requests
                        ? "muted"
                        : ["accent", "violet", "ok", "warn"][i % 4]
                    }
                  />
                </div>
              ))
            )}
            <div style={{ marginTop: 6, paddingTop: 14, borderTop: "1px solid var(--line-mid)" }}>
              <Muted>
                A self-hosted model priced at 0 shows a real $0.00; a model with no price at all
                shows <span style={{ color: "var(--warn-fg)" }}>unpriced</span>. They are different
                facts, and only the first one is free.
              </Muted>
            </div>
          </Card>

          <Card title="Where these numbers come from">
            <Muted>
              Folded from <Mono style={{ color: "var(--fg-2)" }}>usage_events</Mono>, which the data
              plane writes from the tail of each response — one parse at the end of a stream, never
              on the request path. A row exists only for principals whose consumption is tracked,
              because nothing else parses the response body. Per-model metrics on{" "}
              <Mono style={{ color: "var(--fg-2)" }}>/metrics</Mono> cover every request; per-caller
              rows cover those.
            </Muted>
          </Card>
        </Stack>
      </Grid>
    </Stack>
  );
}

function Kpi({ label, value, sub, tone }) {
  return (
    <Card>
      <div
        style={{
          font: "400 11px var(--sans)",
          color: "var(--fg-3)",
          letterSpacing: ".06em",
          marginBottom: 8,
        }}
      >
        {label}
      </div>
      <div style={{ font: "500 24px/1 var(--mono)", letterSpacing: "-.02em" }}>{value}</div>
      {sub && (
        <div
          style={{
            font: "400 11px var(--sans)",
            color: tone === "warn" ? "var(--warn-fg)" : "var(--fg-4)",
            marginTop: 8,
          }}
        >
          {sub}
        </div>
      )}
    </Card>
  );
}
