import React, { useState } from "react";
import { api } from "../api.js";
import { attempt, useLoader } from "../load.js";
import {
  Bar,
  Button,
  Card,
  Dot,
  Empty,
  ErrorNote,
  Field,
  Grid,
  Loading,
  Mono,
  Muted,
  Pill,
  Renamable,
  Row,
  Spacer,
  Stack,
} from "../ui.jsx";

// Rules, read as an operator reads them: in order, first match wins, and each
// rule's conditions AND'd together.
//
// The dry-run panel is the reason this screen is worth building rather than
// editing YAML. "Why did this request route there" is otherwise answerable
// only by reading production logs, and the answer people actually need is a
// rule *index* — "my second rule matched instead of my first" and "my first
// rule matched and points somewhere I did not expect" are different bugs with
// the same symptom.

const CONDITION_LABELS = {
  principals: "principal ∈",
  roles: "role ∈",
  min_prompt_tokens: "prompt_tokens ≥",
  max_prompt_tokens: "prompt_tokens ≤",
  min_max_tokens: "max_tokens ≥",
  max_max_tokens: "max_tokens ≤",
  stream: "stream =",
  min_budget_used_percent: "budget_used% ≥",
  max_budget_used_percent: "budget_used% ≤",
  max_inflight_per_backend: "inflight/backend ≤",
  min_request_cost_micros: "request cost ≥",
  max_request_cost_micros: "request cost ≤",
  after: "after",
  before: "before",
  days: "weekday ∈",
  class: "class =",
};

/**
 * The chips for one rule's conditions.
 *
 * Takes the whole rule, not a `match_condition` field, because there is no
 * such field on the wire: `RuleView` and `NewRule` both put the conditions at
 * the top level with `#[serde(flatten)]`. Reading a nested object here found
 * `undefined` and drew "catch-all" on every rule that had conditions, and
 * posting one buried them where serde discarded them — every rule the UI
 * created matched every request that reached its position.
 */
function conditionChips(rule) {
  const out = [];
  for (const [key, label] of Object.entries(CONDITION_LABELS)) {
    const v = rule?.[key];
    if (v === undefined || v === null) continue;
    if (Array.isArray(v)) {
      if (v.length === 0) continue;
      out.push([label, v.join(", ")]);
    } else if (key.endsWith("_request_cost_micros")) {
      out.push([label, `$${(v / 1e6).toFixed(6).replace(/0+$/, "0")}`]);
    } else {
      out.push([label, String(v)]);
    }
  }
  for (const [name, value] of Object.entries(rule?.headers || {})) {
    out.push(["header", `${name}: ${value}`]);
  }
  return out;
}

export function VirtualModels({ onUnauthorised }) {
  const [selected, setSelected] = useState(null);
  const [creating, setCreating] = useState("");
  const [sim, setSim] = useState(null);

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [vms, models, principals, fallback, pools] = await Promise.all([
        api.get("/admin/frontend-models"),
        api.get("/admin/provider-models"),
        api.get("/admin/principals"),
        api.get("/admin/fallback-model"),
        api.get("/admin/model-pools"),
      ]);
      return { vms, models, principals, fallback, pools };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const vm = data.vms.find((v) => v.id === selected) || data.vms[0] || null;

  const create = async (e) => {
    e.preventDefault();
    if (!creating.trim()) return;
    const ok = await attempt(
      () =>
        api.post("/admin/frontend-models", {
          name: creating.trim(),
          description: "",
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setCreating("");
      reload();
    }
  };

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Grid cols="224px minmax(0,1fr)">
        <Card style={{ padding: 10 }}>
          <div
            style={{
              font: "500 10px var(--sans)",
              color: "var(--fg-5)",
              letterSpacing: ".1em",
              padding: "6px 8px 10px",
            }}
          >
            VIRTUAL MODELS
          </div>
          {data.vms.length === 0 && <Empty>None yet.</Empty>}
          {data.vms.map((v) => {
            const on = vm && v.id === vm.id;
            return (
              <button
                key={v.id}
                onClick={() => {
                  setSelected(v.id);
                  setSim(null);
                }}
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: 3,
                  alignItems: "flex-start",
                  width: "100%",
                  textAlign: "left",
                  background: on ? "var(--accent-bg)" : "none",
                  border: "none",
                  borderRadius: 8,
                  padding: "9px 10px",
                  color: on ? "var(--fg)" : "var(--fg-2)",
                  font: "500 12.5px var(--mono)",
                }}
              >
                {v.name}
                <span
                  style={{ font: "400 10px var(--sans)", color: "var(--fg-5)" }}
                >
                  {v.rules.length} rule{v.rules.length === 1 ? "" : "s"}
                  {v.default_targets.length
                    ? ` · ${v.default_targets.length} default`
                    : ""}
                </span>
              </button>
            );
          })}
          <form onSubmit={create} style={{ marginTop: 8 }}>
            <input
              placeholder="+ new name"
              value={creating}
              onChange={(e) => setCreating(e.target.value)}
              style={{ width: "100%", fontSize: 12 }}
            />
          </form>
        </Card>

        {!vm ? (
          <Card>
            <Empty>
              A frontend model is a name callers ask for; it resolves to a chain
              of provider models. Create one to start.
            </Empty>
          </Card>
        ) : (
          <Stack gap={14}>
            <Card>
              <Row style={{ flexWrap: "nowrap" }}>
                <div style={{ minWidth: 0 }}>
                  <Renamable
                    value={vm.name}
                    style={{ font: "500 15px/1.2 var(--mono)" }}
                    hint="Click to rename. This is the name clients ask for, so renaming it changes your public API — grants follow, callers do not."
                    onSave={(name) =>
                      attempt(
                        () =>
                          api.patch(`/admin/frontend-models/${vm.id}`, {
                            name,
                          }),
                        setError,
                        onUnauthorised,
                      ).then((ok) => ok && reload())
                    }
                  />
                  <div
                    style={{
                      font: "400 11px var(--sans)",
                      color: "var(--fg-4)",
                      marginTop: 4,
                    }}
                  >
                    First rule whose conditions match wins · conditions
                    AND&rsquo;d · targets ordered as a fallback chain
                  </div>
                </div>
                <Spacer />
                <Button
                  onClick={() =>
                    setSim(sim ? null : { streaming: true, prompt_tokens: 0 })
                  }
                >
                  {sim ? "Close dry-run" : "Dry-run a request"}
                </Button>
                <Button
                  variant="smallDanger"
                  onClick={async () => {
                    if (
                      !window.confirm(
                        `Delete ${vm.name}, its rules and targets?`,
                      )
                    )
                      return;
                    const ok = await attempt(
                      () => api.del(`/admin/frontend-models/${vm.id}`),
                      setError,
                      onUnauthorised,
                    );
                    if (ok) {
                      setSelected(null);
                      reload();
                    }
                  }}
                >
                  delete
                </Button>
              </Row>
            </Card>

            {sim && (
              <DryRun
                vm={vm}
                principals={data.principals}
                state={sim}
                setState={setSim}
                onError={setError}
              />
            )}

            {vm.rules.length === 0 && (
              <Card>
                <Empty>
                  No rules. Every request to {vm.name} goes to its defaults
                  below, then to the deployment fallback.
                </Empty>
              </Card>
            )}

            {vm.rules.map((r, i) => {
              const chips = conditionChips(r);
              const total = r.targets.reduce((a, t) => a + t.weight, 0) || 1;
              return (
                <Card
                  key={r.id}
                  style={{ borderLeft: "3px solid var(--accent)" }}
                >
                  <Row
                    gap={10}
                    style={{ marginBottom: 12, flexWrap: "nowrap" }}
                  >
                    <Pill tone="accent" mono>
                      rule {i}
                    </Pill>
                    {r.action === "deny" && (
                      <Pill tone="warn" mono>
                        deny {r.deny_status}
                      </Pill>
                    )}
                    {r.action === "jump" && (
                      <Pill tone="quiet" mono>
                        jump →{" "}
                        {data.vms.find((o) => o.id === r.jump_to)?.name ||
                          "deleted"}
                      </Pill>
                    )}
                    {r.tag && (
                      <Pill tone="quiet" mono>
                        tag {r.tag}
                      </Pill>
                    )}
                    <Muted>
                      {chips.length === 0
                        ? "no conditions — matches everything that reaches it"
                        : `${chips.length} condition${chips.length === 1 ? "" : "s"}, all must hold`}
                    </Muted>
                    <Spacer />
                    <Button
                      variant="smallDanger"
                      onClick={async () => {
                        const ok = await attempt(
                          () => api.del(`/admin/rules/${r.id}`),
                          setError,
                          onUnauthorised,
                        );
                        if (ok) reload();
                      }}
                    >
                      remove
                    </Button>
                  </Row>

                  <Row gap={6} style={{ marginBottom: 12 }}>
                    {chips.length === 0 && <Pill tone="quiet">catch-all</Pill>}
                    {chips.map(([k, v], j) => (
                      <span
                        key={j}
                        style={{
                          background: "var(--panel-2)",
                          border: "1px solid var(--line)",
                          borderRadius: 7,
                          padding: "5px 9px",
                          font: "400 11px var(--mono)",
                          color: "var(--fg-2)",
                        }}
                      >
                        <span style={{ color: "var(--fg-4)" }}>{k}</span>{" "}
                        <span style={{ color: "var(--accent-soft)" }}>{v}</span>
                      </span>
                    ))}
                  </Row>

                  <Row gap={8}>
                    <span
                      style={{
                        font: "500 10px var(--sans)",
                        color: "var(--fg-5)",
                        letterSpacing: ".08em",
                      }}
                    >
                      TARGETS
                    </span>
                    {r.targets.length === 0 && (
                      <Muted>none — this rule matches and serves nothing</Muted>
                    )}
                    {r.targets.map((t, j) => (
                      <React.Fragment key={t.id}>
                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            gap: 8,
                            background: "var(--panel-2)",
                            border: "1px solid var(--line)",
                            borderRadius: 8,
                            padding: "7px 10px",
                          }}
                        >
                          <Dot tone="ok" size={6} />
                          <Mono style={{ font: "400 12px var(--mono)" }}>
                            {t.model}
                            {t.provider_model_id === null ? (
                              <span style={{ color: "var(--warn)" }}>
                                {" · unavailable"}
                              </span>
                            ) : null}
                          </Mono>
                          <span
                            style={{
                              font: "400 10px var(--sans)",
                              color: "var(--fg-4)",
                            }}
                          >
                            w{t.weight}
                          </span>
                          <div style={{ width: 52 }}>
                            <Bar pct={(t.weight / total) * 100} height={4} />
                          </div>
                          <Button
                            variant="small"
                            style={{ border: "none", padding: "0 2px" }}
                            onClick={async () => {
                              const ok = await attempt(
                                () => api.del(`/admin/rule-targets/${t.id}`),
                                setError,
                                onUnauthorised,
                              );
                              if (ok) reload();
                            }}
                          >
                            ×
                          </Button>
                        </div>
                        {j < r.targets.length - 1 && (
                          <span
                            style={{
                              font: "400 11px var(--mono)",
                              color: "#3f4650",
                            }}
                          >
                            →
                          </span>
                        )}
                      </React.Fragment>
                    ))}
                    <AddTarget
                      models={data.models}
                      pools={data.pools}
                      onAdd={async (target) => {
                        const ok = await attempt(
                          () =>
                            api.post(`/admin/rules/${r.id}/targets`, {
                              ...target,
                              position: r.targets.length,
                            }),
                          setError,
                          onUnauthorised,
                        );
                        if (ok) reload();
                      }}
                    />
                  </Row>
                </Card>
              );
            })}

            <AddRule
              vm={{
                ...vm,
                // Every other frontend model, for a `jump` destination. A
                // model cannot jump to itself, and the API refuses a jump
                // that would close a loop.
                siblings: data.vms.filter((o) => o.id !== vm.id),
              }}
              models={data.models}
              onError={setError}
              onDone={reload}
              onUnauthorised={onUnauthorised}
            />

            <Card
              title="Defaults"
              subtitle="used when no rule matches · the deployment fallback is appended after these"
            >
              <Row gap={8}>
                {vm.default_targets.length === 0 && (
                  <Muted>none configured</Muted>
                )}
                {vm.default_targets.map((t) => (
                  <div
                    key={t.id}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 8,
                      background: "var(--panel-2)",
                      border: "1px solid var(--line)",
                      borderRadius: 8,
                      padding: "7px 10px",
                    }}
                  >
                    <Dot tone="ok" size={6} />
                    <Mono style={{ font: "400 12px var(--mono)" }}>
                      {t.model}
                      {t.provider_model_id === null ? (
                        <span style={{ color: "var(--warn)" }}>
                          {" · unavailable"}
                        </span>
                      ) : null}
                    </Mono>
                    <span
                      style={{
                        font: "400 10px var(--sans)",
                        color: "var(--fg-4)",
                      }}
                    >
                      w{t.weight}
                    </span>
                    <Button
                      variant="small"
                      style={{ border: "none", padding: "0 2px" }}
                      onClick={async () => {
                        const ok = await attempt(
                          () =>
                            api.del(`/admin/frontend-model-defaults/${t.id}`),
                          setError,
                          onUnauthorised,
                        );
                        if (ok) reload();
                      }}
                    >
                      ×
                    </Button>
                  </div>
                ))}
                {data.fallback?.name && (
                  <>
                    <span
                      style={{ font: "400 11px var(--mono)", color: "#3f4650" }}
                    >
                      →
                    </span>
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: 8,
                        background: "var(--panel-2)",
                        border: "1px dashed #2b313a",
                        borderRadius: 8,
                        padding: "7px 10px",
                      }}
                    >
                      <Dot tone="muted" size={6} />
                      <Mono
                        style={{
                          font: "400 12px var(--mono)",
                          color: "var(--fg-3)",
                        }}
                      >
                        {data.fallback.name}
                      </Mono>
                      <span
                        style={{
                          font: "400 10px var(--sans)",
                          color: "var(--fg-5)",
                        }}
                      >
                        deployment fallback
                      </span>
                    </div>
                  </>
                )}
                <AddTarget
                  models={data.models}
                  pools={data.pools}
                  onAdd={async (target) => {
                    const ok = await attempt(
                      () =>
                        api.post(`/admin/frontend-models/${vm.id}/defaults`, {
                          ...target,
                          position: vm.default_targets.length,
                        }),
                      setError,
                      onUnauthorised,
                    );
                    if (ok) reload();
                  }}
                />
              </Row>
            </Card>
          </Stack>
        )}
      </Grid>
    </Stack>
  );
}

// A target is a provider model *or* a pool, and the picker offers both — a
// pool is how you point a rule at several models at once, so the choice
// between "one model" and "a load-balanced group" is made right here rather
// than by a policy somewhere else on the page.
function AddTarget({ models, pools, onAdd }) {
  const [open, setOpen] = useState(false);
  const [pick, setPick] = useState("");
  if (!open) {
    return (
      <button
        onClick={() => setOpen(true)}
        style={{
          border: "1px dashed #2b313a",
          background: "none",
          borderRadius: 8,
          padding: "7px 10px",
          font: "400 11px var(--sans)",
          color: "var(--fg-5)",
        }}
      >
        + target
      </button>
    );
  }
  return (
    <Row gap={6} style={{ flexWrap: "nowrap" }}>
      <select
        value={pick}
        onChange={(e) => setPick(e.target.value)}
        style={{ fontSize: 12 }}
      >
        <option value="">model or pool…</option>
        <optgroup label="Pools — several models, one policy">
          {(pools || []).map((p) => (
            <option key={p.id} value={`pool:${p.id}`}>
              {p.name} · {p.members.length} member
              {p.members.length === 1 ? "" : "s"} ·{" "}
              {p.policy || "weighted split"}
            </option>
          ))}
        </optgroup>
        <optgroup label="Provider models">
          {models.map((m) => (
            <option key={m.id} value={`model:${m.id}`}>
              {m.name}
              {m.backends.length === 0
                ? " · no provider"
                : m.backends.length > 1
                  ? ` · ${m.backends.length} providers`
                  : ` · ${m.backends[0].provider_name}`}
            </option>
          ))}
        </optgroup>
      </select>
      <Button
        variant="secondary"
        onClick={() => {
          if (!pick) return;
          const [kind, id] = pick.split(":");
          onAdd(
            kind === "pool" ? { model_pool_id: id } : { provider_model_id: id },
          );
          setOpen(false);
          setPick("");
        }}
      >
        add
      </Button>
      <Button variant="small" onClick={() => setOpen(false)}>
        cancel
      </Button>
    </Row>
  );
}

/**
 * A rule builder over the conditions the routing engine actually evaluates.
 *
 * Only the fields `MatchConditionJson` defines are offered. A free-text JSON
 * box would let an operator write a condition the engine silently ignores,
 * which is the worst failure this screen can have: the rule looks configured
 * and never fires.
 */
function AddRule({ vm, models, onError, onDone, onUnauthorised }) {
  const [open, setOpen] = useState(false);
  const [c, setC] = useState({});
  const set = (patch) => setC({ ...c, ...patch });

  if (!open) {
    return (
      <button
        onClick={() => setOpen(true)}
        style={{
          background: "none",
          border: "1px dashed #2b313a",
          borderRadius: "var(--radius)",
          color: "var(--fg-3)",
          padding: 12,
          font: "500 12px var(--sans)",
        }}
      >
        + Add rule
      </button>
    );
  }

  const submit = async () => {
    const match_condition = {};
    if (c.class) match_condition.class = c.class;
    if (c.roles)
      match_condition.roles = c.roles
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean);
    if (c.min_prompt_tokens)
      match_condition.min_prompt_tokens = Number(c.min_prompt_tokens);
    if (c.max_max_tokens)
      match_condition.max_max_tokens = Number(c.max_max_tokens);
    if (c.stream === "true") match_condition.stream = true;
    if (c.stream === "false") match_condition.stream = false;
    if (c.min_budget_used_percent)
      match_condition.min_budget_used_percent = Number(
        c.min_budget_used_percent,
      );
    if (c.max_inflight_per_backend)
      match_condition.max_inflight_per_backend = Number(
        c.max_inflight_per_backend,
      );
    // Typed in dollars, stored in micro-units — the unit every price in this
    // schema uses.
    if (c.min_request_cost)
      match_condition.min_request_cost_micros = Math.round(
        Number(c.min_request_cost) * 1e6,
      );
    if (c.header_name && c.header_value)
      match_condition.headers = {
        [c.header_name.toLowerCase()]: c.header_value,
      };
    if (c.after) match_condition.after = c.after;
    if (c.before) match_condition.before = c.before;

    const ok = await attempt(
      async () => {
        // Spread, not nested: `NewRule` flattens `MatchConditionJson`, and
        // because every field of it is `#[serde(default)]` with no
        // `deny_unknown_fields`, a nested object deserialises to an empty
        // condition and answers 201 — a catch-all rule with no error anywhere.
        const rule = await api.post(`/admin/frontend-models/${vm.id}/rules`, {
          position: vm.rules.length,
          // Every action is terminal, so this is the whole of what the rule
          // does once it matches — there is no second pass.
          action: c.action || undefined,
          deny_status:
            c.action === "deny" ? Number(c.deny_status || 403) : undefined,
          deny_message: c.action === "deny" ? c.deny_message : undefined,
          jump_to: c.action === "jump" ? c.jump_to : undefined,
          tag: c.tag || undefined,
          ...match_condition,
        });
        if (c.model_id && c.action !== "deny" && c.action !== "jump") {
          await api.post(`/admin/rules/${rule.id}/targets`, {
            provider_model_id: c.model_id,
            weight: 100,
            position: 0,
          });
        }
      },
      onError,
      onUnauthorised,
    );
    if (ok) {
      setOpen(false);
      setC({});
      onDone();
    }
  };

  return (
    <Card title={`New rule — position ${vm.rules.length}`} tone="accent">
      <Muted>
        Appended last, so it is evaluated after every existing rule. Every field
        left blank is simply not part of the condition.
      </Muted>
      <Grid cols={4} gap={10} style={{ marginTop: 12 }}>
        <Field
          label="ACTION"
          hint="route sends it to the targets below; deny refuses; jump continues in another frontend model's rules"
        >
          <select
            value={c.action || ""}
            onChange={(e) => set({ action: e.target.value })}
          >
            <option value="">route — to this rule's targets</option>
            <option value="deny">deny — refuse with a status</option>
            <option value="jump">jump — continue in another chain</option>
          </select>
        </Field>
        {c.action === "deny" && (
          <>
            <Field
              label="DENY STATUS"
              hint="4xx only — a 5xx would tell every client library to retry something that will never be allowed"
            >
              <input
                placeholder="402"
                value={c.deny_status || ""}
                onChange={(e) => set({ deny_status: e.target.value })}
              />
            </Field>
            <Field label="DENY MESSAGE">
              <input
                placeholder="batch keys may not use the big model"
                value={c.deny_message || ""}
                onChange={(e) => set({ deny_message: e.target.value })}
              />
            </Field>
          </>
        )}
        {c.action === "jump" && (
          <Field label="JUMP TO">
            <select
              value={c.jump_to || ""}
              onChange={(e) => set({ jump_to: e.target.value })}
            >
              <option value="">frontend model…</option>
              {(vm.siblings || []).map((o) => (
                <option key={o.id} value={o.id}>
                  {o.name}
                </option>
              ))}
            </select>
          </Field>
        )}
        <Field
          label="TAG"
          hint="carried onto this rule's usage rows, so spend can be attributed to the decision"
        >
          <input
            placeholder="team-research"
            value={c.tag || ""}
            onChange={(e) => set({ tag: e.target.value })}
          />
        </Field>
        <Field label="PROMPT CLASS">
          <input
            placeholder="coding"
            value={c.class || ""}
            onChange={(e) => set({ class: e.target.value })}
          />
        </Field>
        <Field label="ROLES (any of)">
          <input
            placeholder="engineering, batch"
            value={c.roles || ""}
            onChange={(e) => set({ roles: e.target.value })}
          />
        </Field>
        <Field label="STREAM">
          <select
            value={c.stream || ""}
            onChange={(e) => set({ stream: e.target.value })}
          >
            <option value="">any</option>
            <option value="true">true</option>
            <option value="false">false</option>
          </select>
        </Field>
        <Field label="PROMPT TOKENS ≥">
          <input
            value={c.min_prompt_tokens || ""}
            onChange={(e) => set({ min_prompt_tokens: e.target.value })}
          />
        </Field>
        <Field label="MAX_TOKENS ≤">
          <input
            value={c.max_max_tokens || ""}
            onChange={(e) => set({ max_max_tokens: e.target.value })}
          />
        </Field>
        <Field
          label="BUDGET USED % ≥"
          hint="degrade instead of refusing at the cap"
        >
          <input
            value={c.min_budget_used_percent || ""}
            onChange={(e) => set({ min_budget_used_percent: e.target.value })}
          />
        </Field>
        <Field
          label="REQUEST COST ≥ $"
          hint="priced at the cheapest model this frontend model can reach — most useful with deny"
        >
          <input
            placeholder="0.50"
            value={c.min_request_cost || ""}
            onChange={(e) => set({ min_request_cost: e.target.value })}
          />
        </Field>
        <Field label="INFLIGHT/BACKEND ≤">
          <input
            value={c.max_inflight_per_backend || ""}
            onChange={(e) => set({ max_inflight_per_backend: e.target.value })}
          />
        </Field>
        <Field label="FIRST TARGET">
          <select
            value={c.model_id || ""}
            onChange={(e) => set({ model_id: e.target.value })}
          >
            <option value="">none yet</option>
            {models.map((m) => (
              <option key={m.id} value={m.id}>
                {m.name}
                {m.backends.length === 0
                  ? " · no provider"
                  : m.backends.length > 1
                    ? ` · ${m.backends.length} providers`
                    : ` · ${m.backends[0].provider_name}`}
              </option>
            ))}
          </select>
        </Field>
        <Field label="HEADER NAME">
          <input
            placeholder="x-fastllm-tier"
            value={c.header_name || ""}
            onChange={(e) => set({ header_name: e.target.value })}
          />
        </Field>
        <Field label="HEADER VALUE">
          <input
            placeholder="batch"
            value={c.header_value || ""}
            onChange={(e) => set({ header_value: e.target.value })}
          />
        </Field>
        <Field label="AFTER (HH:MM)">
          <input
            placeholder="22:00"
            value={c.after || ""}
            onChange={(e) => set({ after: e.target.value })}
          />
        </Field>
        <Field label="BEFORE (HH:MM)">
          <input
            placeholder="06:00"
            value={c.before || ""}
            onChange={(e) => set({ before: e.target.value })}
          />
        </Field>
      </Grid>
      <Row gap={8} style={{ marginTop: 14 }}>
        <Button variant="primary" onClick={submit}>
          Create rule
        </Button>
        <Button variant="small" onClick={() => setOpen(false)}>
          cancel
        </Button>
      </Row>
    </Card>
  );
}

/**
 * Evaluate the rules without dispatching anything.
 *
 * Two limits are stated on the panel rather than hidden, because both would
 * otherwise make this lie quietly: health is not consulted (the control plane
 * builds a fresh registry, so every backend looks reachable) and the prompt
 * class is supplied rather than computed (this answers what a `coding` prompt
 * would do, not whether some particular prompt is coding).
 */
function DryRun({ vm, principals, state, setState, onError }) {
  const [result, setResult] = useState(null);
  const [busy, setBusy] = useState(false);

  const run = async () => {
    setBusy(true);
    try {
      const headers = {};
      if (state.header_name && state.header_value) {
        headers[state.header_name] = state.header_value;
      }
      const resp = await api.post("/admin/routing/dry-run", {
        model: vm.name,
        principal_id: state.principal_id ? state.principal_id : undefined,
        streaming: !!state.streaming,
        prompt_tokens: Number(state.prompt_tokens) || 0,
        max_tokens: state.max_tokens ? Number(state.max_tokens) : undefined,
        class: state.class || undefined,
        headers,
      });
      setResult(resp);
      onError(null);
    } catch (e) {
      onError(e.message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card tone="accent" style={{ background: "var(--panel-2)" }}>
      <Row gap={10} style={{ marginBottom: 14 }}>
        <span
          style={{ font: "600 12px var(--sans)", color: "var(--accent-soft)" }}
        >
          Dry-run a request
        </span>
        <Muted>evaluates rules without dispatching</Muted>
      </Row>
      <Grid cols={4} gap={10}>
        <Field label="PRINCIPAL">
          <select
            value={state.principal_id || ""}
            onChange={(e) =>
              setState({ ...state, principal_id: e.target.value })
            }
          >
            <option value="">anonymous</option>
            {principals.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </Field>
        <Field label="PROMPT TOKENS">
          <input
            value={state.prompt_tokens ?? ""}
            onChange={(e) =>
              setState({ ...state, prompt_tokens: e.target.value })
            }
          />
        </Field>
        <Field label="STREAM">
          <select
            value={state.streaming ? "true" : "false"}
            onChange={(e) =>
              setState({ ...state, streaming: e.target.value === "true" })
            }
          >
            <option value="true">true</option>
            <option value="false">false</option>
          </select>
        </Field>
        <Field label="CLASS" hint="supplied, not classified">
          <input
            placeholder="coding"
            value={state.class || ""}
            onChange={(e) => setState({ ...state, class: e.target.value })}
          />
        </Field>
        <Field label="MAX TOKENS">
          <input
            value={state.max_tokens || ""}
            onChange={(e) => setState({ ...state, max_tokens: e.target.value })}
          />
        </Field>
        <Field label="HEADER NAME">
          <input
            placeholder="x-fastllm-tier"
            value={state.header_name || ""}
            onChange={(e) =>
              setState({ ...state, header_name: e.target.value })
            }
          />
        </Field>
        <Field label="HEADER VALUE">
          <input
            value={state.header_value || ""}
            onChange={(e) =>
              setState({ ...state, header_value: e.target.value })
            }
          />
        </Field>
        <div style={{ display: "flex", alignItems: "flex-end" }}>
          <Button
            variant="primary"
            onClick={run}
            disabled={busy}
            style={{ width: "100%" }}
          >
            {busy ? "Evaluating…" : "Evaluate"}
          </Button>
        </div>
      </Grid>

      {result && (
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 10,
            padding: 12,
            marginTop: 14,
            background: "var(--panel)",
            border: "1px solid var(--line)",
            borderRadius: 9,
            flexWrap: "wrap",
          }}
        >
          <Pill tone={result.matched_rule === null ? "quiet" : "ok"} mono>
            {result.matched_rule === null
              ? "NO RULE"
              : `RULE ${result.matched_rule}`}
          </Pill>
          <span style={{ font: "400 12px var(--sans)", color: "var(--fg-2)" }}>
            {result.matched_rule === null
              ? "no rule matched — the defaults decided"
              : `rule ${result.matched_rule} matched first`}
            {result.candidates.length > 0 && (
              <>
                {" → "}
                {result.candidates.map((c, i) => (
                  <React.Fragment key={c + i}>
                    {i > 0 && <span style={{ color: "#3f4650" }}> → </span>}
                    <Mono style={{ color: "var(--fg)" }}>{c}</Mono>
                  </React.Fragment>
                ))}
              </>
            )}
          </span>
          <Spacer />
          <Muted>
            health is not consulted here — this says which rule matched, not
            which replica is reachable
          </Muted>
        </div>
      )}
      {result?.denied && (
        <div style={{ marginTop: 10 }}>
          <Muted style={{ color: "var(--warn-fg)" }}>
            Refused by rule {result.matched_rule}: the caller would get{" "}
            <b>{result.denied.status}</b> — {result.denied.message}. This is the
            rule working, not a missing target.
          </Muted>
        </div>
      )}
      {result && !result.denied && result.candidates.length === 0 && (
        <div style={{ marginTop: 10 }}>
          <Muted style={{ color: "var(--warn-fg)" }}>
            The chain is empty: this request would 404. Every candidate was
            either unconfigured or dropped because the caller lacks model:invoke
            on it.
          </Muted>
        </div>
      )}
      {result?.tag && (
        <div style={{ marginTop: 6 }}>
          <Muted>
            Usage for this request would be tagged <b>{result.tag}</b>.
          </Muted>
        </div>
      )}
      {/* The one condition a dry-run cannot answer. It runs on the control
          plane, which builds a registry with no in-flight counters and no
          engine scrape, so a spill rule always looks like it still matches.
          Saying so beats letting an operator conclude their rule is broken. */}
      {result &&
        vm.rules.some(
          (r) =>
            r.max_inflight_per_backend !== undefined &&
            r.max_inflight_per_backend !== null,
        ) && (
          <div style={{ marginTop: 6 }}>
            <Muted style={{ color: "var(--warn-fg)" }}>
              A rule here uses inflight/backend, which reads live load. The
              dry-run runs on the control plane and cannot see it, so that rule
              always evaluates as though every backend were idle — only real
              traffic shows the spill.
            </Muted>
          </div>
        )}
    </Card>
  );
}
