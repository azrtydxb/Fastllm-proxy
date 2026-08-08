import React, { useState } from "react";
import { api } from "../api.js";
import { attempt, useLoader } from "../load.js";
import { mergeBackends, backendKey } from "../fleet.js";
import {
  Button,
  Card,
  Dot,
  Ellipsis,
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
  fmtPrice,
} from "../ui.jsx";

const BACKEND_COLS = [
  { label: "API BASE", width: "2fr" },
  { label: "UPSTREAM MODEL", width: "1.2fr" },
  { label: "PROTOCOL", width: ".7fr" },
  { label: "CREDENTIAL", width: ".8fr" },
  { label: "MAX TOKENS", width: ".8fr", align: "right" },
  { label: "", width: "80px", align: "right" },
];

export function Models({ onUnauthorised }) {
  const [newModel, setNewModel] = useState({ name: "", description: "" });
  const [drafts, setDrafts] = useState({});
  const [editing, setEditing] = useState(null);
  const [sync, setSync] = useState(null);
  const [busy, setBusy] = useState(false);

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [models, fleet] = await Promise.all([
        api.get("/admin/models"),
        api.get("/admin/fleet"),
      ]);
      return { models, fleet };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data) return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const health = new Map(mergeBackends(data.fleet).map((b) => [b.key, b]));
  const draftFor = (id) => drafts[id] || {};
  const setDraft = (id, patch) =>
    setDrafts({ ...drafts, [id]: { ...(drafts[id] || {}), ...patch } });

  const create = async (e) => {
    e.preventDefault();
    if (!newModel.name.trim()) return;
    const ok = await attempt(
      () =>
        api.post("/admin/models", {
          name: newModel.name.trim(),
          description: newModel.description.trim(),
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setNewModel({ name: "", description: "" });
      reload();
    }
  };

  const addBackend = async (modelId) => {
    const d = draftFor(modelId);
    if (!d.api_base) return;
    const ok = await attempt(
      () =>
        api.post(`/admin/models/${modelId}/backends`, {
          api_base: d.api_base.trim(),
          upstream_model: d.upstream_model?.trim() || undefined,
          upstream_api_key: d.upstream_api_key || undefined,
          protocol: d.protocol || undefined,
          default_max_tokens: d.default_max_tokens ? Number(d.default_max_tokens) : undefined,
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setDrafts({ ...drafts, [modelId]: {} });
      reload();
    }
  };

  const savePrices = async (m, patch) => {
    const ok = await attempt(() => api.patch(`/admin/models/${m.id}`, patch), setError, onUnauthorised);
    if (ok) {
      setEditing(null);
      reload();
    }
  };

  const runSync = async (dry) => {
    setBusy(true);
    try {
      const resp = await api.post("/admin/prices/sync", { dry_run: dry, source: "both" });
      setSync({ ...resp, applied: !dry });
      if (!dry) reload();
      setError(null);
    } catch (e) {
      setError(e.message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Row>
        <Muted>
          Concrete models. Prices are per million tokens; caching is opt-in per model. A model with
          no price stores NULL rather than zero, so spend is reported as unpriced instead of free.
        </Muted>
        <Spacer />
        <Button onClick={() => runSync(true)} disabled={busy}>
          {busy ? "Fetching…" : "Sync prices…"}
        </Button>
      </Row>

      {sync && (
        <Card tone={sync.applied ? "accent" : undefined} title={sync.applied ? "Prices applied" : "Price sync — preview"}>
          <Stack gap={12}>
            <Muted>
              {fmtInt(sync.updated)} model{sync.updated === 1 ? "" : "s"} would change ·{" "}
              {fmtInt(sync.already_priced)} already priced and left alone ·{" "}
              {fmtInt(sync.unmatched)} with no match in either catalogue.
            </Muted>
            {(sync.changes || []).length > 0 && (
              <Table cols={SYNC_COLS}>
                {sync.changes.map((c) => (
                  <Tr
                    key={c.model}
                    cols={SYNC_COLS}
                    cells={[
                      <Mono key="m">{c.model}</Mono>,
                      <Mono key="i">{fmtPrice(c.input_price_per_mtok)} / Mtok in</Mono>,
                      <Mono key="o">{fmtPrice(c.output_price_per_mtok)} / Mtok out</Mono>,
                    ]}
                  />
                ))}
              </Table>
            )}
            <Row>
              <Muted>
                Existing prices are never overwritten: a rate you negotiated must not be replaced
                by a list price.
              </Muted>
              <Spacer />
              {!sync.applied && sync.updated > 0 && (
                <Button variant="primary" onClick={() => runSync(false)} disabled={busy}>
                  Apply to {fmtInt(sync.updated)} model{sync.updated === 1 ? "" : "s"}
                </Button>
              )}
              <Button variant="small" onClick={() => setSync(null)}>
                dismiss
              </Button>
            </Row>
          </Stack>
        </Card>
      )}

      <Card title="New model">
        <form onSubmit={create}>
          <Row gap={10} style={{ flexWrap: "nowrap" }}>
            <Field label="NAME" style={{ flex: 1 }}>
              <input
                placeholder="local-qwen"
                value={newModel.name}
                onChange={(e) => setNewModel({ ...newModel, name: e.target.value })}
              />
            </Field>
            <Field label="DESCRIPTION" style={{ flex: 2 }}>
              <input
                placeholder="what callers should know about it"
                value={newModel.description}
                onChange={(e) => setNewModel({ ...newModel, description: e.target.value })}
              />
            </Field>
            <Button variant="primary" type="submit" style={{ alignSelf: "flex-end" }}>
              Create
            </Button>
          </Row>
        </form>
      </Card>

      {data.models.length === 0 && (
        <Card>
          <Empty>No models yet. A model is a name; its backends are where it actually runs.</Empty>
        </Card>
      )}

      {data.models.map((m) => {
        const priced = m.input_price_per_mtok !== null;
        const cached = m.cache_ttl_seconds > 0;
        const edit = editing === m.id;
        return (
          <Card key={m.id} style={{ padding: 0 }}>
            <Row
              gap={10}
              style={{
                padding: "14px 16px",
                borderBottom: "1px solid var(--line-mid)",
                flexWrap: "nowrap",
              }}
            >
              <Mono style={{ font: "500 13px var(--mono)" }}>{m.name}</Mono>
              <Pill tone="quiet" mono>
                id {m.id}
              </Pill>
              <Pill tone={priced ? "neutral" : "warn"} mono>
                {priced
                  ? `${fmtPrice(m.input_price_per_mtok)} / ${fmtPrice(m.output_price_per_mtok)}`
                  : "unpriced"}
              </Pill>
              <Pill tone={cached ? "accent" : "quiet"} mono>
                {cached ? `cache ${m.cache_ttl_seconds}s` : "cache off"}
              </Pill>
              {m.description && <Muted>{m.description}</Muted>}
              <Spacer />
              <Button variant="small" onClick={() => setEditing(edit ? null : m.id)}>
                {edit ? "cancel" : "edit"}
              </Button>
              <Button
                variant="smallDanger"
                onClick={async () => {
                  if (!window.confirm(`Delete ${m.name} and all its backends?`)) return;
                  const ok = await attempt(
                    () => api.del(`/admin/models/${m.id}`),
                    setError,
                    onUnauthorised,
                  );
                  if (ok) reload();
                }}
              >
                delete
              </Button>
            </Row>

            {edit && <PriceEditor model={m} onSave={(patch) => savePrices(m, patch)} />}

            <div style={{ padding: "4px 16px 14px" }}>
              <Table cols={BACKEND_COLS}>
                {m.backends.map((b) => {
                  const h = health.get(backendKey(b.api_base, b.upstream_model || m.name));
                  return (
                    <Tr
                      key={b.id}
                      cols={BACKEND_COLS}
                      cells={[
                        <Row key="b" gap={8} style={{ flexWrap: "nowrap", minWidth: 0 }}>
                          <Dot tone={!h ? "muted" : h.split ? "warn" : h.healthy ? "ok" : "bad"} />
                          <Ellipsis
                            title={h ? undefined : "no proxy has probed this backend yet"}
                            style={{ font: "400 12px var(--mono)", color: "var(--fg-2)" }}
                          >
                            {b.api_base}
                          </Ellipsis>
                        </Row>,
                        <Ellipsis key="u" style={{ font: "400 12px var(--mono)" }}>
                          {b.upstream_model || "—"}
                        </Ellipsis>,
                        <Mono key="p" style={{ font: "400 12px var(--mono)", color: "var(--fg-3)" }}>
                          {b.protocol}
                        </Mono>,
                        <Mono
                          key="c"
                          style={{
                            font: "400 12px var(--mono)",
                            color: b.has_upstream_api_key ? "var(--ok-fg)" : "var(--fg-5)",
                          }}
                        >
                          {b.has_upstream_api_key ? "set" : "—"}
                        </Mono>,
                        <Mono key="t" style={{ font: "400 12px var(--mono)", color: "var(--fg-3)" }}>
                          {b.default_max_tokens ?? "—"}
                        </Mono>,
                        <Button
                          key="x"
                          variant="small"
                          onClick={async () => {
                            const ok = await attempt(
                              () => api.del(`/admin/backends/${b.id}`),
                              setError,
                              onUnauthorised,
                            );
                            if (ok) reload();
                          }}
                        >
                          remove
                        </Button>,
                      ]}
                    />
                  );
                })}
              </Table>

              <Row gap={8} style={{ marginTop: 12, flexWrap: "nowrap" }}>
                <input
                  placeholder="api_base"
                  style={{ flex: 1.8 }}
                  value={draftFor(m.id).api_base || ""}
                  onChange={(e) => setDraft(m.id, { api_base: e.target.value })}
                />
                <input
                  placeholder="upstream_model"
                  style={{ flex: 1.1 }}
                  value={draftFor(m.id).upstream_model || ""}
                  onChange={(e) => setDraft(m.id, { upstream_model: e.target.value })}
                />
                <select
                  style={{ flex: 0.7 }}
                  value={draftFor(m.id).protocol || "openai"}
                  onChange={(e) => setDraft(m.id, { protocol: e.target.value })}
                >
                  <option value="openai">openai</option>
                  <option value="anthropic">anthropic</option>
                  <option value="gemini">gemini</option>
                </select>
                <input
                  placeholder="default_max_tokens"
                  style={{ flex: 0.8 }}
                  value={draftFor(m.id).default_max_tokens || ""}
                  onChange={(e) => setDraft(m.id, { default_max_tokens: e.target.value })}
                />
                <input
                  type="password"
                  placeholder="api key"
                  style={{ flex: 0.8 }}
                  value={draftFor(m.id).upstream_api_key || ""}
                  onChange={(e) => setDraft(m.id, { upstream_api_key: e.target.value })}
                />
                <Button variant="secondary" onClick={() => addBackend(m.id)}>
                  Add backend
                </Button>
              </Row>
              {draftFor(m.id).protocol === "anthropic" && !draftFor(m.id).default_max_tokens && (
                <div style={{ marginTop: 8 }}>
                  <Muted>
                    An Anthropic backend rejects a request with no max_tokens, so this one needs a
                    default — inventing one here would cap generation silently.
                  </Muted>
                </div>
              )}
            </div>
          </Card>
        );
      })}
    </Stack>
  );
}

const SYNC_COLS = [
  { label: "MODEL", width: "1.4fr" },
  { label: "INPUT", width: "1fr" },
  { label: "OUTPUT", width: "1fr" },
];

/**
 * Prices and cache TTL, with `PATCH` semantics made visible.
 *
 * Leaving a field alone and clearing it are different operations — absent
 * means "leave", `null` means "clear" — so the form has an explicit clear
 * rather than treating an emptied box as either one. An emptied box that
 * silently cleared a price would turn a typo into a model that reports its
 * spend as unpriced.
 */
function PriceEditor({ model, onSave }) {
  const [input, setInput] = useState(
    model.input_price_per_mtok === null ? "" : String(model.input_price_per_mtok / 1e6),
  );
  const [output, setOutput] = useState(
    model.output_price_per_mtok === null ? "" : String(model.output_price_per_mtok / 1e6),
  );
  const [ttl, setTtl] = useState(model.cache_ttl_seconds ?? "");
  const [description, setDescription] = useState(model.description || "");

  const micros = (v) => (v === "" ? undefined : Math.round(Number(v) * 1e6));

  return (
    <div style={{ padding: "14px 16px", borderBottom: "1px solid var(--line-mid)" }}>
      <Grid cols={4} gap={10}>
        <Field label="INPUT $ / MTOK">
          <input value={input} onChange={(e) => setInput(e.target.value)} placeholder="3.00" />
        </Field>
        <Field label="OUTPUT $ / MTOK">
          <input value={output} onChange={(e) => setOutput(e.target.value)} placeholder="15.00" />
        </Field>
        <Field label="CACHE TTL (s)" hint="0 or empty turns the response cache off for this model">
          <input value={ttl} onChange={(e) => setTtl(e.target.value)} placeholder="300" />
        </Field>
        <Field label="DESCRIPTION">
          <input value={description} onChange={(e) => setDescription(e.target.value)} />
        </Field>
      </Grid>
      <Row gap={8} style={{ marginTop: 12 }}>
        <Button
          variant="primary"
          onClick={() =>
            onSave({
              input_price_per_mtok: micros(input),
              output_price_per_mtok: micros(output),
              cache_ttl_seconds: ttl === "" ? undefined : Number(ttl),
              description,
            })
          }
        >
          Save
        </Button>
        <Button
          variant="danger"
          onClick={() =>
            onSave({ input_price_per_mtok: null, output_price_per_mtok: null })
          }
          title="Clear both prices — spend for this model will report as unpriced"
        >
          Clear prices
        </Button>
        <Muted>
          A saved change rebuilds and republishes the snapshot immediately; running requests are
          unaffected.
        </Muted>
      </Row>
    </div>
  );
}
