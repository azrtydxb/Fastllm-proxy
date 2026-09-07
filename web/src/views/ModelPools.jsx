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
const POLICIES = [
  [
    "cache-affinity",
    "cache affinity — a conversation returns to the member holding its cache",
  ],
  ["least-loaded", "least loaded — fewest in-flight requests"],
  ["lowest-latency", "lowest latency — when members are not equally fast"],
  ["round-robin", "round robin — strict rotation"],
  ["cheapest", "cheapest — lowest published price; unpriced ranks last"],
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
  const [creating, setCreating] = useState("");
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

      <Card title="New pool">
        <Row gap={8} style={{ flexWrap: "nowrap" }}>
          <input
            placeholder="leastloaded-gemma"
            value={creating}
            onChange={(e) => setCreating(e.target.value)}
            style={{ flex: 1 }}
          />
          <Button
            variant="primary"
            disabled={!creating.trim()}
            onClick={async () => {
              const ok = await attempt(
                () =>
                  api.post("/admin/model-pools", {
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
            }}
          >
            Create
          </Button>
        </Row>
        <div style={{ marginTop: 8 }}>
          <Muted>
            A pool needs a name of its own — routing resolves targets by name,
            so it cannot share one with a provider model or a frontend model.
          </Muted>
        </div>
      </Card>

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
