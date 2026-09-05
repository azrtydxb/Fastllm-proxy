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
  fmtCompact,
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
  // Without this a model that already has a price can never be re-synced from
  // the UI — including one sitting at a wrong 0, which reads as "free" and is
  // exactly the case somebody wants to fix.
  const [overwrite, setOverwrite] = useState(false);
  const [busy, setBusy] = useState(false);
  // What each provider answered `GET /v1/models` with, keyed by provider id.
  // Kept per provider rather than per model form so browsing once serves every
  // model being attached to the same endpoint — OpenRouter answers with
  // several hundred, and asking it again per form would be a page of requests
  // for a list that does not change.
  const [served, setServed] = useState({});

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [models, fleet, catalogue, providers] = await Promise.all([
        api.get("/admin/provider-models"),
        api.get("/admin/fleet"),
        api.get("/admin/provider-catalogue"),
        api.get("/admin/providers"),
      ]);
      return { models, fleet, catalogue, providers };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const health = new Map(mergeBackends(data.fleet).map((b) => [b.key, b]));
  const draftFor = (id) => drafts[id] || {};
  const setDraft = (id, patch) =>
    setDrafts({ ...drafts, [id]: { ...(drafts[id] || {}), ...patch } });

  const create = async (e) => {
    e.preventDefault();
    if (!newModel.name.trim()) return;
    const ok = await attempt(
      () =>
        api.post("/admin/provider-models", {
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
    if (!d.provider_id && !d.api_base) return;
    // Naming a provider settles the endpoint and the credential, so the fields
    // that describe one are not sent — the API refuses them alongside a
    // provider_id rather than quietly preferring one source over the other.
    const body = d.provider_id
      ? { provider_id: Number(d.provider_id) }
      : {
          api_base: d.api_base.trim(),
          upstream_api_key: d.upstream_api_key || undefined,
          protocol: d.protocol || undefined,
        };
    const ok = await attempt(
      () =>
        api.post(`/admin/provider-models/${modelId}/backends`, {
          ...body,
          upstream_model: d.upstream_model?.trim() || undefined,
          default_max_tokens: d.default_max_tokens
            ? Number(d.default_max_tokens)
            : undefined,
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setDrafts({ ...drafts, [modelId]: {} });
      reload();
    }
  };

  /**
   * Ask a provider what it serves.
   *
   * Nothing is created by this — it is the same `GET /v1/models` the sweep
   * runs, so the list is what that endpoint will actually answer to rather
   * than what a catalogue believes it offers.
   */
  const browse = async (providerId) => {
    setServed({ ...served, [providerId]: { loading: true } });
    try {
      const resp = await api.get(
        `/admin/providers/${providerId}/available-models`,
      );
      setServed({ ...served, [providerId]: { models: resp.models } });
      setError(null);
    } catch (e) {
      // Reported in place rather than in the page's error slot: a provider
      // that does not implement /v1/models is a fact about that row, and the
      // name can still be typed.
      setServed({ ...served, [providerId]: { error: e.message } });
    }
  };

  const savePrices = async (m, patch) => {
    const ok = await attempt(
      () => api.patch(`/admin/provider-models/${m.id}`, patch),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setEditing(null);
      reload();
    }
  };

  // `force` is passed rather than read from state: the checkbox re-previews
  // in the same tick it changes, and `setOverwrite` has not landed yet.
  const runSync = async (dry, force = overwrite) => {
    setBusy(true);
    try {
      const resp = await api.post("/admin/prices/sync", {
        dry_run: dry,
        source: "both",
        overwrite: force,
      });
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
          Provider models: what requests are actually routed to. One name on one
          provider — the same model on two hosts is two provider models, and a
          frontend model is what balances between them. Prices are per million
          tokens; caching is opt-in per model. A model with no price stores NULL
          rather than zero, so spend is reported as unpriced instead of free.
        </Muted>
        <Spacer />
        <Button onClick={() => runSync(true)} disabled={busy}>
          {busy ? "Fetching…" : "Sync prices…"}
        </Button>
      </Row>

      {sync && (
        <Card
          tone={sync.applied ? "accent" : undefined}
          title={sync.applied ? "Prices applied" : "Price sync — preview"}
        >
          <Stack gap={12}>
            <Muted>
              {fmtInt(sync.updated)} model{sync.updated === 1 ? "" : "s"} would
              change · {fmtInt(sync.already_priced)} already priced and left
              alone · {fmtInt(sync.unmatched)} with no match in either
              catalogue.
            </Muted>
            {(sync.changes || []).length > 0 && (
              <Table cols={SYNC_COLS}>
                {sync.changes.map((c) => (
                  <Tr
                    key={c.model}
                    cols={SYNC_COLS}
                    cells={[
                      <Mono key="m">{c.model}</Mono>,
                      <Mono key="i">
                        {fmtPrice(c.input_price_per_mtok)} / Mtok in
                      </Mono>,
                      <Mono key="o">
                        {fmtPrice(c.output_price_per_mtok)} / Mtok out
                      </Mono>,
                    ]}
                  />
                ))}
              </Table>
            )}
            <Row>
              <label style={{ display: "flex", gap: 8, alignItems: "center" }}>
                <input
                  type="checkbox"
                  checked={overwrite}
                  onChange={(e) => {
                    setOverwrite(e.target.checked);
                    // Re-preview immediately: the counts and the list on
                    // screen were computed under the old setting, and leaving
                    // them there would make the Apply button claim a number
                    // it is no longer about to do.
                    runSync(true, e.target.checked);
                  }}
                />
                <Muted>
                  Replace prices that are already set. Off by default: a rate
                  you negotiated must not be replaced by a list price. On, when
                  a model is priced wrongly — a stale figure, or a 0 that reads
                  as free.
                </Muted>
              </label>
              <Spacer />
              {!sync.applied && sync.updated > 0 && (
                <Button
                  variant="primary"
                  onClick={() => runSync(false)}
                  disabled={busy}
                >
                  Apply to {fmtInt(sync.updated)} model
                  {sync.updated === 1 ? "" : "s"}
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
                onChange={(e) =>
                  setNewModel({ ...newModel, name: e.target.value })
                }
              />
            </Field>
            <Field label="DESCRIPTION" style={{ flex: 2 }}>
              <input
                placeholder="what callers should know about it"
                value={newModel.description}
                onChange={(e) =>
                  setNewModel({ ...newModel, description: e.target.value })
                }
              />
            </Field>
            <Button
              variant="primary"
              type="submit"
              style={{ alignSelf: "flex-end" }}
            >
              Create
            </Button>
          </Row>
        </form>
      </Card>

      {data.models.length === 0 && (
        <Card>
          <Empty>
            No models yet. A model is a name; its backends are where it actually
            runs.
          </Empty>
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
              {/* Shown even when undeclared, because "we do not know" is the
                  state that changes routing: a model with no declared window
                  is never demoted for being too small, and an operator
                  looking at an oversized-prompt problem needs to see which
                  models that applies to. */}
              <Pill tone={m.context_length ? "quiet" : "warn"} mono>
                {m.context_length
                  ? `${fmtCompact(m.context_length)} ctx`
                  : "ctx undeclared"}
              </Pill>
              {m.description && <Muted>{m.description}</Muted>}
              <Spacer />
              <Button
                variant="small"
                onClick={() => setEditing(edit ? null : m.id)}
              >
                {edit ? "cancel" : "edit"}
              </Button>
              <Button
                variant="smallDanger"
                onClick={async () => {
                  if (!window.confirm(`Delete ${m.name} and all its backends?`))
                    return;
                  const ok = await attempt(
                    () => api.del(`/admin/provider-models/${m.id}`),
                    setError,
                    onUnauthorised,
                  );
                  if (ok) reload();
                }}
              >
                delete
              </Button>
            </Row>

            {edit && (
              <PriceEditor model={m} onSave={(patch) => savePrices(m, patch)} />
            )}

            <div style={{ padding: "4px 16px 14px" }}>
              <Table cols={BACKEND_COLS}>
                {m.backends.map((b) => {
                  const h = health.get(
                    backendKey(b.api_base, b.upstream_model || m.name),
                  );
                  return (
                    <Tr
                      key={b.id}
                      cols={BACKEND_COLS}
                      cells={[
                        <Row
                          key="b"
                          gap={8}
                          style={{ flexWrap: "nowrap", minWidth: 0 }}
                        >
                          <Dot
                            tone={
                              !h
                                ? "muted"
                                : h.split
                                  ? "warn"
                                  : h.healthy
                                    ? "ok"
                                    : "bad"
                            }
                          />
                          <Ellipsis
                            title={
                              h
                                ? undefined
                                : "no proxy has probed this backend yet"
                            }
                            style={{
                              font: "400 12px var(--mono)",
                              color: "var(--fg-2)",
                            }}
                          >
                            {b.api_base}
                          </Ellipsis>
                        </Row>,
                        <Ellipsis
                          key="u"
                          style={{ font: "400 12px var(--mono)" }}
                        >
                          {b.upstream_model || "—"}
                        </Ellipsis>,
                        <Mono
                          key="p"
                          style={{
                            font: "400 12px var(--mono)",
                            color: "var(--fg-3)",
                          }}
                        >
                          {b.protocol}
                        </Mono>,
                        <Mono
                          key="c"
                          style={{
                            font: "400 12px var(--mono)",
                            color: b.has_upstream_api_key
                              ? "var(--ok-fg)"
                              : "var(--fg-5)",
                          }}
                        >
                          {b.has_upstream_api_key ? "set" : "—"}
                        </Mono>,
                        <Mono
                          key="t"
                          style={{
                            font: "400 12px var(--mono)",
                            color: "var(--fg-3)",
                          }}
                        >
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

              {/* A provider model has exactly one provider since migration
                  0029, so offering this form on a model that already has one
                  would only earn a 409. Detaching is how you change it, and
                  two upstreams for one client-facing name is now a frontend
                  model with two targets. */}
              {m.backends.length > 0 ? (
                <div style={{ marginTop: 12 }}>
                  <Muted>
                    Served by {m.provider_name}. A model has one provider —
                    remove this one to point it elsewhere, or put this and
                    another behind a frontend model to balance across them.
                  </Muted>
                </div>
              ) : (
                <Stack gap={8} style={{ marginTop: 12 }}>
                  {/* Which provider serves it comes first, because it settles
                      everything else: an existing provider already carries the
                      address, the protocol and the credential, and can be
                      asked what it serves. */}
                  <Row gap={8} style={{ flexWrap: "nowrap" }}>
                    <select
                      style={{ flex: 1.6 }}
                      value={draftFor(m.id).provider_id || ""}
                      onChange={(e) => {
                        setDraft(m.id, {
                          provider_id: e.target.value,
                          upstream_model: "",
                        });
                        // Asked for on selection rather than behind a button:
                        // choosing a provider is the question "what does it
                        // have", and the answer is cached per provider, so
                        // this is one request however many models are being
                        // attached to it.
                        if (e.target.value && !served[e.target.value]) {
                          browse(e.target.value);
                        }
                      }}
                    >
                      <option value="">a new endpoint…</option>
                      {(data.providers || []).map((p) => (
                        <option key={p.id} value={p.id}>
                          {p.name} — {p.api_base}
                        </option>
                      ))}
                    </select>

                    {draftFor(m.id).provider_id ? (
                      (() => {
                        const pid = draftFor(m.id).provider_id;
                        const list = served[pid];
                        return (
                          <>
                            {list?.models ? (
                              <select
                                style={{ flex: 1.6 }}
                                value={draftFor(m.id).upstream_model || ""}
                                onChange={(e) =>
                                  setDraft(m.id, {
                                    upstream_model: e.target.value,
                                  })
                                }
                              >
                                <option value="">
                                  upstream model… ({list.models.length} served)
                                </option>
                                {list.models.map((x) => (
                                  <option
                                    key={x.upstream_model}
                                    value={x.upstream_model}
                                  >
                                    {x.upstream_model}
                                    {x.registered
                                      ? " · already registered"
                                      : ""}
                                  </option>
                                ))}
                              </select>
                            ) : (
                              <input
                                placeholder="upstream_model"
                                style={{ flex: 1.6 }}
                                value={draftFor(m.id).upstream_model || ""}
                                onChange={(e) =>
                                  setDraft(m.id, {
                                    upstream_model: e.target.value,
                                  })
                                }
                              />
                            )}
                            <Button
                              variant="secondary"
                              disabled={list?.loading}
                              onClick={() => browse(pid)}
                            >
                              {list?.loading
                                ? "Asking…"
                                : list?.models
                                  ? "Refresh list"
                                  : "Browse models"}
                            </Button>
                          </>
                        );
                      })()
                    ) : (
                      <>
                        {/* Picking a known provider fills in the base URL and
                            the header its vendor wants the key in. The
                            catalogue is not a limit — anything speaking the
                            OpenAI API works whether or not it is listed — so
                            the address stays typeable. */}
                        <select
                          style={{ flex: 0.9 }}
                          value={draftFor(m.id).catalogue_key || ""}
                          onChange={(e) => {
                            const entry = (data.catalogue || []).find(
                              (c) => c.key === e.target.value,
                            );
                            setDraft(m.id, {
                              catalogue_key: e.target.value,
                              ...(entry
                                ? {
                                    api_base: entry.base_url,
                                    protocol: entry.protocol,
                                  }
                                : {}),
                            });
                          }}
                        >
                          <option value="">provider…</option>
                          {(data.catalogue || []).map((c) => (
                            <option key={c.key} value={c.key}>
                              {c.display_name}
                            </option>
                          ))}
                        </select>
                        <input
                          placeholder="api_base"
                          style={{ flex: 1.8 }}
                          value={draftFor(m.id).api_base || ""}
                          onChange={(e) =>
                            setDraft(m.id, { api_base: e.target.value })
                          }
                        />
                        <input
                          placeholder="upstream_model"
                          style={{ flex: 1.1 }}
                          value={draftFor(m.id).upstream_model || ""}
                          onChange={(e) =>
                            setDraft(m.id, { upstream_model: e.target.value })
                          }
                        />
                        <select
                          style={{ flex: 0.7 }}
                          value={draftFor(m.id).protocol || "openai"}
                          onChange={(e) =>
                            setDraft(m.id, { protocol: e.target.value })
                          }
                        >
                          <option value="openai">openai</option>
                          <option value="anthropic">anthropic</option>
                          <option value="gemini">gemini</option>
                        </select>
                        <input
                          type="password"
                          placeholder="api key"
                          style={{ flex: 0.8 }}
                          value={draftFor(m.id).upstream_api_key || ""}
                          onChange={(e) =>
                            setDraft(m.id, {
                              upstream_api_key: e.target.value,
                            })
                          }
                        />
                      </>
                    )}

                    <input
                      placeholder="default_max_tokens"
                      style={{ flex: 0.8 }}
                      value={draftFor(m.id).default_max_tokens || ""}
                      onChange={(e) =>
                        setDraft(m.id, { default_max_tokens: e.target.value })
                      }
                    />
                    <Button
                      variant="secondary"
                      onClick={() => addBackend(m.id)}
                    >
                      Attach provider
                    </Button>
                  </Row>

                  {served[draftFor(m.id).provider_id]?.error && (
                    <Muted>
                      {served[draftFor(m.id).provider_id].error} — type the
                      upstream model name instead.
                    </Muted>
                  )}
                </Stack>
              )}
              {draftFor(m.id).protocol === "anthropic" &&
                !draftFor(m.id).default_max_tokens && (
                  <div style={{ marginTop: 8 }}>
                    <Muted>
                      An Anthropic backend rejects a request with no max_tokens,
                      so this one needs a default — inventing one here would cap
                      generation silently.
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
    model.input_price_per_mtok === null
      ? ""
      : String(model.input_price_per_mtok / 1e6),
  );
  const [output, setOutput] = useState(
    model.output_price_per_mtok === null
      ? ""
      : String(model.output_price_per_mtok / 1e6),
  );
  const [ttl, setTtl] = useState(model.cache_ttl_seconds ?? "");
  const [context, setContext] = useState(model.context_length ?? "");
  const [description, setDescription] = useState(model.description || "");

  // Absent, cleared and mistyped are three different things and only the
  // first two are intentional. `JSON.stringify` renders NaN as `null`, and to
  // `PATCH` a null means *clear* — so "3,50" or a stray character would have
  // turned a priced model into an unpriced one, which is exactly the failure
  // this editor's doc comment claims to guard against.
  const [bad, setBad] = useState(null);

  // Same contract as `micros`/`seconds` above: throw on anything that is not
  // a clean positive integer, so a typo surfaces as a refusal instead of
  // silently clearing the field it was meant to set.
  const positive = (raw, what) => {
    const n = Number(String(raw).trim());
    if (!Number.isInteger(n) || n <= 0) throw new Error(`${raw}" for ${what}`);
    return n;
  };
  const micros = (v) => {
    if (v.trim() === "") return undefined;
    const n = Number(v);
    if (!Number.isFinite(n) || n < 0) throw new RangeError(v);
    return Math.round(n * 1e6);
  };
  const seconds = (v) => {
    if (v === "" || v === null) return null;
    const n = Number(v);
    if (!Number.isInteger(n) || n < 0) throw new RangeError(v);
    return n;
  };

  return (
    <div
      style={{
        padding: "14px 16px",
        borderBottom: "1px solid var(--line-mid)",
      }}
    >
      <Grid cols={4} gap={10}>
        <Field label="INPUT $ / MTOK">
          <input
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder="3.00"
          />
        </Field>
        <Field label="OUTPUT $ / MTOK">
          <input
            value={output}
            onChange={(e) => setOutput(e.target.value)}
            placeholder="15.00"
          />
        </Field>
        <Field
          label="CACHE TTL (s)"
          hint="0 or empty turns the response cache off for this model"
        >
          <input
            value={ttl}
            onChange={(e) => setTtl(e.target.value)}
            placeholder="300"
          />
        </Field>
        <Field
          label="CONTEXT LENGTH"
          hint="tokens this model accepts · empty means undeclared, which routing treats as unknown rather than unlimited"
        >
          <input
            value={context}
            onChange={(e) => setContext(e.target.value)}
            placeholder="262144"
          />
        </Field>
        <Field label="DESCRIPTION">
          <input
            value={description}
            onChange={(e) => setDescription(e.target.value)}
          />
        </Field>
      </Grid>
      {bad && (
        <div style={{ marginTop: 12 }}>
          <ErrorNote onDismiss={() => setBad(null)}>{bad}</ErrorNote>
        </div>
      )}
      <Row gap={8} style={{ marginTop: 12 }}>
        <Button
          variant="primary"
          onClick={() => {
            try {
              setBad(null);
              onSave({
                input_price_per_mtok: micros(input),
                output_price_per_mtok: micros(output),
                // Empty means "no TTL", which the hint promises and which only
                // an explicit null delivers: `PATCH` treats an absent field as
                // "leave alone", so omitting it here would silently keep the
                // cache on for a model somebody just turned it off for.
                cache_ttl_seconds: seconds(ttl),
                // Empty clears it back to undeclared. Deliberately not sent
                // as 0: the handler refuses a non-positive length, because a
                // model that accepts no tokens is not a thing and coercing it
                // would leave an operator believing they had set a limit.
                context_length:
                  context === "" ? null : positive(context, "context length"),
                // Empty is an explicit null: it clears the override so the
                description,
              });
            } catch (e) {
              setBad(
                `"${e.message}" is not a number. Nothing was saved — a value that cannot be read would have cleared the field, not left it alone.`,
              );
            }
          }}
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
          A saved change rebuilds and republishes the snapshot immediately;
          running requests are unaffected.
        </Muted>
      </Row>
    </div>
  );
}
