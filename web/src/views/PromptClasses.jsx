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
  Pill,
  Row,
  Spacer,
  Stack,
  Table,
  Tr,
  fmtInt,
} from "../ui.jsx";

// A class is a name plus example prompts. There is no training step — the
// control plane averages the examples into a centroid — so the only quality
// signal is leave-one-out evaluation, and it lives behind a button here.
//
// `routable` is the field that matters most and the easiest to miss: a class
// with examples but no centroid means this control plane has no classifier
// model, and a rule naming that class silently never matches. It is a pill on
// every row for that reason.

const COLS = [
  { label: "CLASS", width: "1.2fr" },
  { label: "TIER", width: ".7fr" },
  { label: "EXAMPLES", width: ".6fr", align: "right" },
  { label: "ROUTABLE", width: ".9fr" },
  { label: "MIN MARGIN", width: ".7fr", align: "right" },
  { label: "REFINES", width: "1fr" },
  { label: "", width: "70px", align: "right" },
];

export function PromptClasses({ onUnauthorised, config }) {
  const [draft, setDraft] = useState({ name: "", tier: "fast", examples: "" });
  const [evaluation, setEvaluation] = useState(null);
  const [busy, setBusy] = useState(false);
  const [addTo, setAddTo] = useState({});

  const { data, error, loading, reload, setError } = useLoader(
    () => api.get("/admin/prompt-classes"),
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const create = async (e) => {
    e.preventDefault();
    if (!draft.name.trim()) return;
    const examples = draft.examples
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    const ok = await attempt(
      () =>
        api.post("/admin/prompt-classes", {
          name: draft.name.trim(),
          description: "",
          tier: draft.tier,
          examples,
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setDraft({ name: "", tier: "fast", examples: "" });
      reload();
    }
  };

  const evaluate = async () => {
    setBusy(true);
    try {
      setEvaluation(await api.post("/admin/prompt-classes/evaluate"));
      setError(null);
    } catch (e) {
      setError(e.message);
    } finally {
      setBusy(false);
    }
  };

  const unroutable = data.filter((c) => !c.routable && c.examples > 0);

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Row>
        <Muted>
          A class is a name plus example prompts. No training step — the control
          plane averages the examples into a centroid, and classification is
          nearest-centroid on the last user message.
        </Muted>
        <Spacer />
        <Button onClick={evaluate} disabled={busy}>
          {busy ? "Evaluating…" : "Run evaluation"}
        </Button>
      </Row>

      {config && !config.classifier_tier1 && (
        <Card tone="warn" title="This build has no classifier">
          <Muted>
            Classes can be defined and stored, but nothing embeds them, so every
            rule naming a class will never match. The <Mono>classifier</Mono>{" "}
            feature and a <Mono>--classifier-model</Mono> are both required.
          </Muted>
        </Card>
      )}

      {unroutable.length > 0 && config?.classifier_tier1 && (
        <Card
          tone="warn"
          title={`${unroutable.length} class${unroutable.length === 1 ? "" : "es"} cannot route`}
        >
          <Muted>
            <Mono>{unroutable.map((c) => c.name).join(", ")}</Mono> have
            examples but no centroid. A rule naming one of them silently never
            matches — which looks exactly like a rule that is simply not being
            hit.
          </Muted>
        </Card>
      )}

      <Grid cols="minmax(0,1fr) 340px">
        <Card>
          {data.length === 0 ? (
            <Empty>
              No classes defined. Without one, routing rules cannot mention a
              class.
            </Empty>
          ) : (
            <Table cols={COLS}>
              {data.map((c) => (
                <React.Fragment key={c.id}>
                  <Tr
                    cols={COLS}
                    cells={[
                      <Mono key="n" style={{ font: "500 12px var(--mono)" }}>
                        {c.name}
                      </Mono>,
                      <Pill key="t" tone={c.tier === "fast" ? "ok" : "violet"}>
                        {c.tier}
                      </Pill>,
                      <Mono key="e" style={{ color: "var(--fg-3)" }}>
                        {fmtInt(c.examples)}
                      </Mono>,
                      <Pill key="r" tone={c.routable ? "ok" : "warn"}>
                        {c.routable ? "routable" : "no centroid"}
                      </Pill>,
                      <Mono key="m" style={{ color: "var(--fg-3)" }}>
                        {c.min_margin ?? "—"}
                      </Mono>,
                      <Mono
                        key="f"
                        style={{ color: "var(--fg-4)", fontSize: 11 }}
                      >
                        {c.refines.length ? c.refines.join(", ") : "—"}
                      </Mono>,
                      <Button
                        key="x"
                        variant="smallDanger"
                        onClick={async () => {
                          if (
                            !window.confirm(
                              `Delete class ${c.name} and its examples?`,
                            )
                          )
                            return;
                          const ok = await attempt(
                            () => api.del(`/admin/prompt-classes/${c.id}`),
                            setError,
                            onUnauthorised,
                          );
                          if (ok) reload();
                        }}
                      >
                        delete
                      </Button>,
                    ]}
                  />
                  <Row
                    gap={8}
                    style={{ padding: "8px 0 14px", flexWrap: "nowrap" }}
                  >
                    <input
                      placeholder={`add an example prompt to ${c.name}`}
                      value={addTo[c.id] || ""}
                      onChange={(e) =>
                        setAddTo({ ...addTo, [c.id]: e.target.value })
                      }
                      style={{ flex: 1, fontSize: 12 }}
                    />
                    <Button
                      variant="small"
                      onClick={async () => {
                        const prompt = (addTo[c.id] || "").trim();
                        if (!prompt) return;
                        const ok = await attempt(
                          () =>
                            api.post(`/admin/prompt-classes/${c.id}/examples`, {
                              prompt,
                            }),
                          setError,
                          onUnauthorised,
                        );
                        if (ok) {
                          setAddTo({ ...addTo, [c.id]: "" });
                          reload();
                        }
                      }}
                    >
                      add
                    </Button>
                  </Row>
                </React.Fragment>
              ))}
            </Table>
          )}
        </Card>

        <Stack gap={14}>
          <Card title="New class">
            <form onSubmit={create}>
              <Stack gap={10}>
                <Field label="NAME">
                  <input
                    placeholder="coding"
                    value={draft.name}
                    onChange={(e) =>
                      setDraft({ ...draft, name: e.target.value })
                    }
                  />
                </Field>
                <Field
                  label="TIER"
                  hint="the refined tier loads a transformer, and only if a rule names a refined class"
                >
                  <select
                    value={draft.tier}
                    onChange={(e) =>
                      setDraft({ ...draft, tier: e.target.value })
                    }
                  >
                    <option value="fast">fast · static embedding</option>
                    <option value="refined">refined · transformer</option>
                  </select>
                </Field>
                <Field
                  label="EXAMPLE PROMPTS"
                  hint="one per line — bare prompts, as a caller would send them"
                >
                  <textarea
                    rows={6}
                    value={draft.examples}
                    onChange={(e) =>
                      setDraft({ ...draft, examples: e.target.value })
                    }
                    style={{ resize: "vertical", fontFamily: "var(--sans)" }}
                  />
                </Field>
                <Button variant="primary" type="submit">
                  Create
                </Button>
              </Stack>
            </form>
          </Card>

          <Card title="Two tiers, and what they cost">
            <Stack gap={10}>
              <TierRow
                name="Fast · static embedding"
                cost="~150 µs"
                desc="Separates subject matter. Always loaded when the feature is built."
                pct={100}
                tone="ok"
              />
              <TierRow
                name="Refined · transformer"
                cost="~13 ms"
                desc="Same subject, different intent. Loaded only if a rule names a refined class."
                pct={12}
                tone="violet"
              />
              <Muted>
                Classification runs on the last user message, not the whole
                body: a system prompt cost two thirds of recall, and by the
                fourth turn of a conversation the class was undetectable.
              </Muted>
            </Stack>
          </Card>
        </Stack>
      </Grid>

      {evaluation && (
        <Evaluation report={evaluation} onDismiss={() => setEvaluation(null)} />
      )}
    </Stack>
  );
}

function TierRow({ name, cost, desc, pct, tone }) {
  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        gap: 6,
        paddingBottom: 10,
        borderBottom: "1px solid var(--line-soft)",
      }}
    >
      <Row style={{ flexWrap: "nowrap" }}>
        <span style={{ font: "500 12px var(--sans)" }}>{name}</span>
        <Spacer />
        <Mono style={{ font: "500 13px var(--mono)" }}>{cost}</Mono>
      </Row>
      <Muted>{desc}</Muted>
      <Bar pct={pct} tone={tone} height={5} />
    </div>
  );
}

/**
 * Leave-one-out evaluation, shown in full.
 *
 * Overall accuracy is deliberately not the headline: a class that is a small
 * share of traffic can fail completely while accuracy barely moves, which is
 * the base-rate trap that hid a total classifier failure in this codebase
 * once already. Precision, recall and the nearest neighbour per class are what
 * an operator can act on.
 */
function Evaluation({ report, onDismiss }) {
  const classes = report.classes || [];
  if (classes.length === 0) {
    return (
      <Card title="Evaluation" tone="warn">
        <Muted>{report.note || "Nothing could be evaluated."}</Muted>
        <div style={{ marginTop: 10 }}>
          <Button variant="small" onClick={onDismiss}>
            dismiss
          </Button>
        </div>
      </Card>
    );
  }
  const cols = [
    { label: "CLASS", width: "1fr" },
    { label: "TIER", width: ".6fr" },
    { label: "EXAMPLES", width: ".6fr", align: "right" },
    { label: "PRECISION", width: "1.1fr" },
    { label: "RECALL", width: "1.1fr" },
    { label: "NEAREST", width: "1.2fr" },
    { label: "VERDICT", width: "1.6fr" },
  ];
  return (
    <Card
      title="Leave-one-out evaluation"
      subtitle="every example scored against centroids that exclude it — scoring against a centroid containing the example inflates every number"
      right={
        <Button variant="small" onClick={onDismiss}>
          dismiss
        </Button>
      }
    >
      <Table cols={cols}>
        {classes.map((c) => {
          const collides = c.collides;
          return (
            <Tr
              key={c.class}
              cols={cols}
              cells={[
                <Mono key="n" style={{ font: "500 12px var(--mono)" }}>
                  {c.class}
                </Mono>,
                <Pill key="t" tone={c.tier === "fast" ? "ok" : "violet"}>
                  {c.tier}
                </Pill>,
                <Mono key="e" style={{ color: "var(--fg-3)" }}>
                  {c.examples}
                </Mono>,
                <Meter key="p" value={c.precision} />,
                <Meter key="r" value={c.recall} />,
                <Mono
                  key="x"
                  style={{
                    fontSize: 11,
                    color: collides ? "var(--warn-fg)" : "var(--fg-4)",
                  }}
                >
                  {c.nearest?.[0]
                    ? `${c.nearest[0][0]} ${c.nearest[0][1].toFixed(2)}`
                    : "—"}
                </Mono>,
                <span
                  key="v"
                  style={{
                    font: "400 11px var(--sans)",
                    color: collides
                      ? "var(--warn-fg)"
                      : c.precision >= 0.85
                        ? "var(--ok-fg)"
                        : "var(--fg-3)",
                  }}
                >
                  {c.verdict}
                </span>,
              ]}
            />
          );
        })}
      </Table>
      {(report.confusions || []).length > 0 && (
        <div
          style={{
            marginTop: 14,
            paddingTop: 12,
            borderTop: "1px solid var(--line-mid)",
          }}
        >
          <Muted>
            Examples that went the wrong way — which prompt, not just that one
            did:
          </Muted>
          <Stack gap={6} style={{ marginTop: 8 }}>
            {report.confusions.slice(0, 8).map((c, i) => (
              <div
                key={i}
                style={{ font: "400 11px var(--sans)", color: "var(--fg-3)" }}
              >
                <Mono style={{ color: "var(--fg-2)" }}>{c.actual}</Mono>
                <span style={{ color: "var(--fg-5)" }}> → </span>
                <Mono style={{ color: "var(--warn-fg)" }}>{c.predicted}</Mono>
                <span style={{ color: "var(--fg-4)" }}>
                  {" "}
                  · &ldquo;{c.example}&rdquo;
                </span>
              </div>
            ))}
          </Stack>
        </div>
      )}
    </Card>
  );
}

function Meter({ value }) {
  const pct = Math.round((value || 0) * 100);
  return (
    <Row gap={8} style={{ flexWrap: "nowrap" }}>
      <Bar
        pct={pct}
        height={5}
        tone={pct >= 90 ? "ok" : pct >= 75 ? "accent" : "warn"}
      />
      <Mono
        style={{
          font: "400 11px var(--mono)",
          color: "var(--fg-2)",
          width: 34,
        }}
      >
        {pct}%
      </Mono>
    </Row>
  );
}
