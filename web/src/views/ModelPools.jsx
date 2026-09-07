import React, { useState } from "react";
import { api } from "../api.js";
import { attempt, useLoader } from "../load.js";
import {
  Button,
  Card,
  Empty,
  ErrorNote,
  Field,
  Loading,
  Mono,
  Muted,
  Pill,
  Renamable,
  Row,
  ShortId,
  Spacer,
  Stack,
} from "../ui.jsx";

// A pool is a named group of provider models and one policy for choosing
// between them. Naming it is the point: `leastloaded-gemma` and
// `lowestlatency-gemma` can hold the same members and differ only in how they
// choose, and either can be pointed at from as many rules as like — which a
// policy buried in one rule's target list could never do.
//
// Three levels, each set in exactly one place:
//   a rule's targets   an ordered failover chain, tried in order
//   a pool             how to choose between several models at once
//   a provider model   how to choose between its own providers
//
// Every policy here reads how busy or how warm a member is *now*. Cost is not
// one of them: a price is fixed, so a balancer set to "cheapest" never
// balanced -- it picked the same member until somebody edited a price. Cost
// decides routing through a rule's `min/max_request_cost_micros` condition.
const POLICIES = [
  [
    "cache-affinity",
    "cache affinity — a conversation returns to the member holding its cache",
  ],
  ["least-loaded", "least loaded — fewest in-flight requests"],
  ["lowest-latency", "lowest latency — when members are not equally fast"],
  ["round-robin", "round robin — strict rotation"],
];

// Where a model actually runs, for the picker and the member chips. A name on
// its own does not say: `bge-m3` is one model on two Sparks, and an operator
// choosing a pool member needs to see which machines they are pulling in.
function where(model) {
  if (!model || model.backends.length === 0)
    return "no provider — not routable";
  return model.backends.map((b) => b.provider_name).join(", ");
}

export function ModelPools({ onUnauthorised }) {
  const [adding, setAdding] = useState({});

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [pools, models] = await Promise.all([
        api.get("/admin/model-pools"),
        api.get("/admin/provider-models"),
      ]);
      return { pools, models };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const run = (fn) =>
    attempt(fn, setError, onUnauthorised).then((ok) => ok && reload());

  return (
    <Stack gap={14}>
      {error && <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>}

      <NewPool
        models={data.models}
        taken={new Set(data.pools.map((p) => p.name))}
        onCreate={async (name, policy, memberIds) => {
          const ok = await attempt(
            async () => {
              const pool = await api.post("/admin/model-pools", {
                name,
                description: "",
                policy: policy || undefined,
              });
              // Members after the pool, in the order they were ticked: that
              // order is the weighted split's declaration order and the
              // failover order within the pool.
              for (const [position, id] of memberIds.entries()) {
                await api.post(`/admin/model-pools/${pool.id}/members`, {
                  provider_model_id: id,
                  weight: 100,
                  position,
                });
              }
            },
            setError,
            onUnauthorised,
          );
          if (ok) reload();
          return ok;
        }}
      />

      {data.pools.length === 0 && (
        <Card>
          <Empty>
            No pools yet. A pool is several provider models chosen between by
            one policy — point a rule at it instead of listing the models.
          </Empty>
        </Card>
      )}

      {data.pools.map((p) => {
        const inPool = new Set(p.members.map((m) => m.provider_model_id));
        const available = data.models.filter((m) => !inPool.has(m.id));
        return (
          <Card key={p.id} style={{ padding: 0 }}>
            <Row
              gap={10}
              style={{
                padding: "14px 16px",
                borderBottom: "1px solid var(--line-mid)",
                flexWrap: "nowrap",
              }}
            >
              <Renamable
                value={p.name}
                style={{ font: "500 13px var(--mono)" }}
                hint="Click to rename. Targets pointing at this pool follow the rename."
                onSave={(name) =>
                  run(() => api.patch(`/admin/model-pools/${p.id}`, { name }))
                }
              />
              <ShortId id={p.id} />
              <Pill tone={p.members.length ? "neutral" : "warn"} mono>
                {p.members.length} member{p.members.length === 1 ? "" : "s"}
              </Pill>
              <select
                value={p.policy || ""}
                onChange={(e) =>
                  run(() =>
                    api.patch(`/admin/model-pools/${p.id}`, {
                      policy: e.target.value === "" ? null : e.target.value,
                    }),
                  )
                }
                style={{ maxWidth: 420 }}
              >
                <option value="">
                  weighted split — by member weight, deterministic per
                  conversation
                </option>
                {POLICIES.map(([value, label]) => (
                  <option key={value} value={value}>
                    {label}
                  </option>
                ))}
              </select>
              <Spacer />
              <Button
                variant="smallDanger"
                onClick={() => {
                  if (!window.confirm(`Delete the pool ${p.name}?`)) return;
                  run(() => api.del(`/admin/model-pools/${p.id}`));
                }}
              >
                delete
              </Button>
            </Row>

            <div style={{ padding: "12px 16px 14px" }}>
              <Row gap={8}>
                {p.members.length === 0 && (
                  <Muted>no members — this pool routes nowhere</Muted>
                )}
                {p.members.map((m) => (
                  <div
                    key={m.id}
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
                    <Mono style={{ font: "400 12px var(--mono)" }}>
                      {m.model}
                    </Mono>
                    {/* Which machines this member brings with it. A model can
                        be served by several providers, and the pool is
                        choosing between models — that second level is the
                        model's own policy, set on Provider models. */}
                    <Muted style={{ font: "400 10px var(--mono)" }}>
                      {where(
                        data.models.find((x) => x.id === m.provider_model_id),
                      )}
                    </Muted>
                    {/* Weight only bites on the weighted split; the other
                        policies read load, latency or price instead. */}
                    <Muted style={{ font: "400 10px var(--mono)" }}>
                      w{m.weight}
                    </Muted>
                    <Button
                      variant="small"
                      onClick={() =>
                        run(() => api.del(`/admin/model-pool-members/${m.id}`))
                      }
                    >
                      ×
                    </Button>
                  </div>
                ))}
              </Row>

              <Row gap={8} style={{ marginTop: 10, flexWrap: "nowrap" }}>
                <Field label="ADD MEMBER">
                  <select
                    value={adding[p.id] || ""}
                    onChange={(e) =>
                      setAdding({ ...adding, [p.id]: e.target.value })
                    }
                  >
                    <option value="">provider model…</option>
                    {available.map((m) => (
                      <option key={m.id} value={m.id}>
                        {m.name} · {where(m)}
                      </option>
                    ))}
                  </select>
                </Field>
                <div style={{ display: "flex", alignItems: "flex-end" }}>
                  <Button
                    variant="secondary"
                    disabled={!adding[p.id]}
                    onClick={async () => {
                      const ok = await attempt(
                        () =>
                          api.post(`/admin/model-pools/${p.id}/members`, {
                            provider_model_id: adding[p.id],
                            weight: 100,
                            position: p.members.length,
                          }),
                        setError,
                        onUnauthorised,
                      );
                      if (ok) {
                        setAdding({ ...adding, [p.id]: "" });
                        reload();
                      }
                    }}
                  >
                    Add
                  </Button>
                </div>
              </Row>
              {/* The question this screen kept raising: one model served by
                  two machines looks like two things, and adding it looks like
                  adding one. Both are true, at different levels. */}
              <div style={{ marginTop: 8 }}>
                <Muted>
                  A member is a <b>model</b>, and brings every provider serving
                  it. This pool chooses between members; which provider serves a
                  given member is that model&rsquo;s own load balancing, set on
                  Provider&nbsp;models.
                </Muted>
              </div>
            </div>
          </Card>
        );
      })}
    </Stack>
  );
}

/**
 * Create a pool by choosing what goes in it, not by naming it first.
 *
 * Naming came first before, which is the wrong order: the name describes the
 * choice, so it cannot be written until the choice is made. Pick the members,
 * pick the policy, and the name falls out of both — still editable, because a
 * generated name is a starting point and not a rule.
 */
function NewPool({ models, taken, onCreate }) {
  const [open, setOpen] = useState(false);
  const [picked, setPicked] = useState([]);
  const [policy, setPolicy] = useState("");
  const [name, setName] = useState("");
  // Once the name is typed in, it stops following the selection: a generated
  // value that overwrote what somebody wrote would be a data-loss bug wearing
  // a convenience hat.
  const [edited, setEdited] = useState(false);

  const suggest = (ids, pol) => {
    if (ids.length === 0) return "";
    const short = (pol || "weighted").replace(/-/g, "");
    const names = ids
      .map((id) => models.find((m) => m.id === id)?.name)
      .filter(Boolean);
    const head = names.slice(0, 2).join("-");
    const rest = names.length > 2 ? `+${names.length - 2}` : "";
    return `${short}-${head}${rest}`;
  };

  const retitle = (ids, pol) => {
    if (!edited) setName(suggest(ids, pol));
  };

  const toggle = (id) => {
    const next = picked.includes(id)
      ? picked.filter((x) => x !== id)
      : [...picked, id];
    setPicked(next);
    retitle(next, policy);
  };

  const clash = name.trim() !== "" && taken.has(name.trim());

  if (!open) {
    return (
      <Card>
        <Row gap={10}>
          <Button variant="primary" onClick={() => setOpen(true)}>
            Create pool
          </Button>
          <Muted>
            Several provider models chosen between by one policy. Point a rule
            at it instead of listing the models.
          </Muted>
        </Row>
      </Card>
    );
  }

  return (
    <Card title="New pool" tone="accent">
      <Stack gap={12}>
        <Field
          label="MEMBERS"
          hint="tick every model this pool may serve — the order you tick them is the order it falls back through"
        >
          <Stack gap={4}>
            {models.map((m) => (
              <label
                key={m.id}
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: 8,
                  font: "400 12px var(--sans)",
                  cursor: "pointer",
                }}
              >
                <input
                  type="checkbox"
                  checked={picked.includes(m.id)}
                  onChange={() => toggle(m.id)}
                />
                <Mono style={{ font: "400 12px var(--mono)" }}>{m.name}</Mono>
                <Muted style={{ font: "400 10px var(--mono)" }}>
                  {where(m)}
                </Muted>
              </label>
            ))}
          </Stack>
        </Field>

        <Field
          label="POLICY"
          hint="how this pool chooses between its members, per request"
        >
          <select
            value={policy}
            onChange={(e) => {
              setPolicy(e.target.value);
              retitle(picked, e.target.value);
            }}
          >
            <option value="">
              weighted split — by member weight, deterministic per conversation
            </option>
            {POLICIES.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </Field>

        <Field
          label="NAME"
          hint="generated from the policy and the members; edit it and it stops following them"
        >
          <input
            value={name}
            placeholder="pick members to generate a name"
            onChange={(e) => {
              setEdited(true);
              setName(e.target.value);
            }}
          />
        </Field>

        {clash && (
          <Muted style={{ color: "var(--warn-fg)" }}>
            A pool called {name.trim()} already exists — routing resolves
            targets by name, so each needs its own.
          </Muted>
        )}

        <Row gap={8}>
          <Button
            variant="primary"
            disabled={picked.length === 0 || !name.trim() || clash}
            onClick={async () => {
              const ok = await onCreate(name.trim(), policy, picked);
              if (ok) {
                setOpen(false);
                setPicked([]);
                setPolicy("");
                setName("");
                setEdited(false);
              }
            }}
          >
            Save
          </Button>
          <Button onClick={() => setOpen(false)}>cancel</Button>
          <Muted>
            {picked.length === 0
              ? "no members yet — a pool with none routes nowhere"
              : `${picked.length} member${picked.length === 1 ? "" : "s"}`}
          </Muted>
        </Row>
      </Stack>
    </Card>
  );
}
