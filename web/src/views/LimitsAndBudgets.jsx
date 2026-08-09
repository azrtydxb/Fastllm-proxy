import React, { useState } from "react";
import { api } from "../api.js";
import { attempt, useLoader } from "../load.js";
import {
  Bar,
  Button,
  Card,
  Empty,
  ErrorNote,
  Field,
  Grid,
  Loading,
  Mono,
  Muted,
  NotAvailable,
  Pill,
  Row,
  Spacer,
  Stack,
  Table,
  Tr,
  fmtCompact,
  fmtMoney,
} from "../ui.jsx";

// Two different mechanisms that are easy to confuse, kept on one screen
// precisely so the difference is visible:
//
// - A **rate limit** paces a caller. Hitting it is a 429, and retrying later
//   works.
// - A **budget** caps consumption over a window. Hitting it is a 402, and only
//   the window rolling over or an operator raising the cap helps.
//
// A caller that sees 429 should back off; a caller that sees 402 should stop.
// The copy here says so, because the two are otherwise indistinguishable from
// the outside and the wrong response to either wastes everyone's time.

const LIMIT_COLS = [
  { label: "PRINCIPAL", width: "1.4fr" },
  { label: "REQ/MIN", width: ".8fr", align: "right" },
  { label: "TOK/MIN", width: ".8fr", align: "right" },
  { label: "", width: "80px", align: "right" },
];

export function LimitsAndBudgets({ onUnauthorised }) {
  const [limitDraft, setLimitDraft] = useState({});
  const [budgetDraft, setBudgetDraft] = useState({ window: "daily" });

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [limits, budgets, principals] = await Promise.all([
        api.get("/admin/limits"),
        api.get("/admin/budgets"),
        api.get("/admin/principals"),
      ]);
      return { limits, budgets, principals };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data) return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  // Both routes are `PUT`: they replace the row, so a field left out is set
  // to NULL rather than left alone. That is coherent for a PUT and surprising
  // in a form, so selecting a principal loads its current values in — an
  // operator adding a request limit to a principal that already had a token
  // limit was silently removing the token limit.
  const loadLimit = (id) => {
    const existing = (data?.limits || []).find((l) => String(l.principal_id) === String(id));
    setLimitDraft({
      principal_id: id,
      requests_per_min: existing?.requests_per_min ?? "",
      tokens_per_min: existing?.tokens_per_min ?? "",
    });
  };
  const loadBudget = (id) => {
    const existing = (data?.budgets || []).find((b) => String(b.principal_id) === String(id));
    setBudgetDraft({
      principal_id: id,
      window: existing?.window || "daily",
      tokens_total: existing?.tokens_total ?? "",
      cost_total: existing?.cost_total_micros == null ? "" : existing.cost_total_micros / 1e6,
    });
  };


  const setLimit = async () => {
    if (!limitDraft.principal_id) return;
    const body = {};
    if (limitDraft.requests_per_min) body.requests_per_min = Number(limitDraft.requests_per_min);
    if (limitDraft.tokens_per_min) body.tokens_per_min = Number(limitDraft.tokens_per_min);
    if (!Object.keys(body).length) {
      setError("A limit needs a request rate, a token rate, or both.");
      return;
    }
    const ok = await attempt(
      () => api.put(`/admin/principals/${limitDraft.principal_id}/limits`, body),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setLimitDraft({});
      reload();
    }
  };

  const setBudget = async () => {
    if (!budgetDraft.principal_id) return;
    const body = { window: budgetDraft.window };
    if (budgetDraft.tokens_total) body.tokens_total = Number(budgetDraft.tokens_total);
    if (budgetDraft.cost_total) body.cost_total_micros = Math.round(Number(budgetDraft.cost_total) * 1e6);
    if (!body.tokens_total && !body.cost_total_micros) {
      setError("A budget needs tokens, money, or both.");
      return;
    }
    const ok = await attempt(
      () => api.put(`/admin/principals/${budgetDraft.principal_id}/budget`, body),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setBudgetDraft({ window: budgetDraft.window });
      reload();
    }
  };

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      {data.budgets.length > 0 && (
        <Grid cols={3}>
          {data.budgets.map((b) => (
            <BudgetCard
              key={b.principal_id}
              budget={b}
              onClear={async () => {
                const ok = await attempt(
                  () => api.del(`/admin/principals/${b.principal_id}/budget`),
                  setError,
                  onUnauthorised,
                );
                if (ok) reload();
              }}
            />
          ))}
        </Grid>
      )}

      <Card title="Set a budget">
        <Row gap={10} style={{ flexWrap: "nowrap", alignItems: "flex-end" }}>
          <Field label="PRINCIPAL" style={{ flex: 1.2 }}>
            <select
              value={budgetDraft.principal_id || ""}
              onChange={(e) => loadBudget(e.target.value)}
            >
              <option value="">principal…</option>
              {data.principals.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          </Field>
          <Field label="TOKENS TOTAL" style={{ flex: 1 }}>
            <input
              placeholder="optional"
              value={budgetDraft.tokens_total || ""}
              onChange={(e) => setBudgetDraft({ ...budgetDraft, tokens_total: e.target.value })}
            />
          </Field>
          <Field label="SPEND CAP ($)" style={{ flex: 1 }}>
            <input
              placeholder="optional"
              value={budgetDraft.cost_total || ""}
              onChange={(e) => setBudgetDraft({ ...budgetDraft, cost_total: e.target.value })}
            />
          </Field>
          <Field label="WINDOW" style={{ width: 130 }}>
            <select
              value={budgetDraft.window}
              onChange={(e) => setBudgetDraft({ ...budgetDraft, window: e.target.value })}
            >
              <option value="daily">daily</option>
              <option value="weekly">weekly</option>
              <option value="monthly">monthly</option>
            </select>
          </Field>
          <Button variant="primary" onClick={setBudget}>
            Set budget
          </Button>
        </Row>
        <div style={{ marginTop: 10 }}>
          <Muted>
            A budget needs tokens, money, or both. A request is refused when either cap is reached,
            and the 402 names which one. Windows are fixed-length — 1 / 7 / 30 days, not calendar
            months. Updating a budget leaves consumption and the window start alone, but{" "}
            <strong style={{ fontWeight: 500, color: "var(--fg-3)" }}>
              replaces both caps with what is in these boxes
            </strong>{" "}
            — selecting a principal loads its current values so an empty box means you meant it.
          </Muted>
        </div>
      </Card>

      <Grid cols="minmax(0,1fr) minmax(0,1fr)">
        <Card
          title="Rate limits"
          subtitle="enforced without a database call on the request path — each replica enforces its reconciled share"
        >
          {data.limits.length === 0 ? (
            <Empty>No principal has a rate limit. Absent means unlimited, not zero.</Empty>
          ) : (
            <Table cols={LIMIT_COLS}>
              {data.limits.map((l) => (
                <Tr
                  key={l.principal_id}
                  cols={LIMIT_COLS}
                  cells={[
                    <Mono key="p" style={{ color: "var(--fg)" }}>
                      {l.principal}
                    </Mono>,
                    <Mono key="r" style={{ color: "var(--fg-2)" }}>
                      {l.requests_per_min ?? "—"}
                    </Mono>,
                    <Mono key="t" style={{ color: "var(--fg-2)" }}>
                      {l.tokens_per_min === null ? "—" : fmtCompact(l.tokens_per_min)}
                    </Mono>,
                    <Row key="x" gap={6} style={{ justifyContent: "flex-end" }}>
                      <Button variant="small" onClick={() => loadLimit(String(l.principal_id))}>
                        edit
                      </Button>
                      <Button
                        variant="smallDanger"
                        onClick={async () => {
                          const ok = await attempt(
                            () => api.del(`/admin/principals/${l.principal_id}/limits`),
                            setError,
                            onUnauthorised,
                          );
                          if (ok) reload();
                        }}
                      >
                        remove
                      </Button>
                    </Row>,
                  ]}
                />
              ))}
            </Table>
          )}
          <Row gap={8} style={{ marginTop: 12, flexWrap: "nowrap" }}>
            <select
              style={{ flex: 1.3 }}
              value={limitDraft.principal_id || ""}
              onChange={(e) => loadLimit(e.target.value)}
            >
              <option value="">principal…</option>
              {data.principals.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
            <input
              placeholder="req/min"
              style={{ flex: 0.8 }}
              value={limitDraft.requests_per_min || ""}
              onChange={(e) => setLimitDraft({ ...limitDraft, requests_per_min: e.target.value })}
            />
            <input
              placeholder="tok/min"
              style={{ flex: 0.8 }}
              value={limitDraft.tokens_per_min || ""}
              onChange={(e) => setLimitDraft({ ...limitDraft, tokens_per_min: e.target.value })}
            />
            <Button variant="secondary" onClick={setLimit}>
              Set
            </Button>
          </Row>
          <div style={{ marginTop: 10 }}>
            <Muted>
              How often a limit actually bit is{" "}
              <NotAvailable why="Rejections are counted in the limiter and exposed on each proxy's /metrics as fastllm_rejections_total. The admin API stores configuration, not counters.">
                not stored here
              </NotAvailable>{" "}
              — <Mono>fastllm_rejections_total</Mono> on a proxy&rsquo;s /metrics has it.
            </Muted>
          </div>
        </Card>

        <Card title="429 and 402 are different answers" subtitle="and a caller should react to them differently">
          <Stack gap={12}>
            <Step
              at="429"
              tone="accent"
              title="Rate limited — pacing"
              desc="The token bucket is empty. Retrying after x-ratelimit-reset works, and a well-behaved client backs off on its own."
            />
            <Step
              at="402"
              tone="bad"
              title="Budget exhausted — spending"
              desc="The window's cap is reached. Retrying does not help; only the window rolling over or an operator raising the cap does. The response names which cap it was."
            />
            <Step
              at="≥80%"
              tone="warn"
              title="Degrade before refusing"
              desc="min_budget_used_percent is a routing condition like any other, so a rule can move a nearly-exhausted principal to a cheaper model instead of refusing it."
            />
            <Muted>
              Degradation is a routing rule, not a special case — which is why there is no switch
              for it here. It lives on the virtual model whose traffic should degrade.
            </Muted>
          </Stack>
        </Card>
      </Grid>
    </Stack>
  );
}

function Step({ at, title, desc, tone }) {
  return (
    <Row gap={12} style={{ alignItems: "flex-start", flexWrap: "nowrap", paddingBottom: 12, borderBottom: "1px solid var(--line-soft)" }}>
      <Pill tone={tone} mono>
        {at}
      </Pill>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ font: "500 12px var(--sans)", color: "var(--fg)" }}>{title}</div>
        <Muted>{desc}</Muted>
      </div>
    </Row>
  );
}

/**
 * One budget, with the *binding* cap highlighted.
 *
 * A principal with both a token cap and a spend cap is refused by whichever it
 * reaches first, so the headline is the higher of the two percentages and the
 * bar for it is the one that gets colour. Showing them as equals would leave
 * the operator to work out which one is about to bite.
 */
function BudgetCard({ budget, onClear }) {
  const tokPct =
    budget.tokens_total > 0 ? Math.min(100, (budget.tokens_used / budget.tokens_total) * 100) : null;
  const costPct =
    budget.cost_total_micros > 0
      ? Math.min(100, (budget.cost_used_micros / budget.cost_total_micros) * 100)
      : null;
  const caps = [tokPct, costPct].filter((x) => x !== null);
  const binding = caps.length ? Math.max(...caps) : 0;
  const tone = binding >= 80 ? "warn" : binding >= 60 ? "accent" : "ok";
  const color =
    tone === "warn" ? "var(--warn-fg)" : tone === "accent" ? "var(--accent)" : "var(--ok-fg)";

  return (
    <Card>
      <Stack gap={12}>
        <Row style={{ flexWrap: "nowrap" }}>
          <Mono style={{ font: "500 13px var(--mono)" }}>{budget.principal}</Mono>
          <Spacer />
          <Pill tone="quiet">{budget.window}</Pill>
        </Row>
        <div style={{ font: "500 22px/1 var(--mono)", color }}>
          {caps.length ? `${binding.toFixed(0)}%` : "—"}
        </div>
        <BudgetRow
          label="TOKENS"
          used={fmtCompact(budget.tokens_used)}
          total={budget.tokens_total === null ? "no token cap" : fmtCompact(budget.tokens_total)}
          pct={tokPct}
          binding={tokPct !== null && tokPct === binding}
          tone={tone}
        />
        <BudgetRow
          label="SPEND"
          used={fmtMoney(budget.cost_used_micros)}
          total={
            budget.cost_total_micros === null ? "no spend cap" : fmtMoney(budget.cost_total_micros)
          }
          pct={costPct}
          binding={costPct !== null && costPct === binding}
          tone={tone}
        />
        <Row style={{ paddingTop: 4, borderTop: "1px solid var(--line-mid)", flexWrap: "nowrap" }}>
          <span style={{ font: "400 10px var(--sans)", color: "var(--fg-5)" }}>
            window opened {new Date(budget.window_start).toLocaleDateString()}
          </span>
          <Spacer />
          <Button variant="small" onClick={onClear}>
            remove
          </Button>
        </Row>
      </Stack>
    </Card>
  );
}

function BudgetRow({ label, used, total, pct, binding, tone }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 5 }}>
      <Row style={{ flexWrap: "nowrap", alignItems: "baseline" }}>
        <span
          style={{
            font: "500 10px var(--sans)",
            letterSpacing: ".06em",
            color: binding ? "var(--warn-fg)" : "var(--fg-4)",
          }}
        >
          {label}
        </span>
        <Spacer />
        <Mono style={{ font: "400 11px var(--mono)", color: "var(--fg-3)" }}>
          {used} / {total}
        </Mono>
      </Row>
      {pct === null ? (
        // A cap that does not exist is drawn as a hatch, not an empty bar: an
        // empty bar reads as "0% used", which is a different fact.
        <div
          style={{
            height: 6,
            borderRadius: 3,
            background:
              "repeating-linear-gradient(90deg,var(--line-mid) 0 6px,#141922 6px 12px)",
          }}
        />
      ) : (
        <Bar pct={pct} tone={binding ? tone : "muted"} />
      )}
    </div>
  );
}
