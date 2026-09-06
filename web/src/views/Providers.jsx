import React, { useState } from "react";
import { api } from "../api.js";
import { attempt, useLoader } from "../load.js";
import { mergeBackends, backendKey } from "../fleet.js";
import {
  Button,
  Chip,
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
  fmtInt,
} from "../ui.jsx";

// A provider is a row in `providers`, read from `GET /admin/providers`.
//
// It used to be a grouping this screen invented, by bucketing every backend
// whose api_base shared an origin. That answered the right question but could
// not be named, counted or referred to by anything else — which is why a
// registration service had nothing to register. Since migration 0029 it is a
// record, and this screen reads it rather than deriving it.
//
// Models are still fetched, for two things the provider row does not carry:
// which model names ride on it, and health, which is per (api_base,
// upstream_model) on each proxy's own report.
//
// What is deliberately absent: region and latency. Neither is modelled
// anywhere. Region would have to be parsed out of a hostname for the two
// clouds that encode it and guessed for the rest, and latency is per process
// on each proxy's own histogram. Inventing either would put a number on this
// page that no query could reproduce.

const FILTERS = ["All", "Hosted", "Self-hosted", "Native protocol"];

/**
 * The name an address will get if the Name box is left empty.
 *
 * Shown as the placeholder rather than described, because "host:port" reads as
 * a format the operator is being asked to follow instead of the default they
 * are about to accept. Mirrors `host_of` in `src/control/api.rs`; being wrong
 * here shows a placeholder that differs from the name the row ends up with,
 * which is why the rule is one line in both places rather than a parse.
 */
function derivedName(apiBase) {
  const rest = apiBase.split("://")[1];
  if (!rest) return "";
  return rest.split("/")[0];
}

// `dynamic` is the one that reads as a state rather than a label: it holds a
// lease, and losing that lease is what eventually removes it.
const KIND_TONE = { dynamic: "accent", cloud: "violet", static: "quiet" };

const BLANK = {
  mode: "cloud",
  catalogue_key: "",
  name: "",
  api_base: "",
  protocol: "openai",
  upstream_api_key: "",
  credential_kind: "static",
};

export function Providers({ onUnauthorised, go }) {
  const [filter, setFilter] = useState("All");
  const [search, setSearch] = useState("");
  const [draft, setDraft] = useState(null);
  const [rotating, setRotating] = useState(null);
  const [key, setKey] = useState("");
  const [editing, setEditing] = useState(null);
  const [edit, setEdit] = useState({});
  const [renaming, setRenaming] = useState(null);
  const [newName, setNewName] = useState("");
  // The catalogue is eighty-odd entries: long enough that finding one by
  // scrolling is slower than typing three letters of its name.
  const [catFilter, setCatFilter] = useState("");

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [providers, models, fleet, catalogue] = await Promise.all([
        api.get("/admin/providers"),
        api.get("/admin/provider-models"),
        api.get("/admin/fleet"),
        api.get("/admin/provider-catalogue"),
      ]);
      return { providers, models, fleet, catalogue };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const health = new Map(mergeBackends(data.fleet).map((b) => [b.key, b]));
  const entry = (data.catalogue || []).find(
    (c) => c.key === draft?.catalogue_key,
  );
  // A typed address could be any endpoint, Vertex included, so it keeps both.
  // A catalogue entry offers what it declares — one value for all but Vertex,
  // which is why the control disappears rather than showing a dropdown with a
  // single option in it.
  // The chosen entry stays visible even when the filter would drop it, so a
  // narrow filter never makes the current selection look lost.
  const catalogue = (data.catalogue || []).filter(
    (c) =>
      c.key === draft?.catalogue_key ||
      c.display_name.toLowerCase().includes(catFilter.trim().toLowerCase()),
  );

  const kinds =
    draft?.mode === "custom"
      ? ["static", "gcp_service_account"]
      : entry
        ? entry.credential_kinds
        : ["static"];

  const create = async (e) => {
    e.preventDefault();
    const cloud = draft.mode === "cloud";
    if (cloud && !draft.catalogue_key) return;
    if (!draft.api_base.trim()) return;
    const ok = await attempt(
      () =>
        api.post("/admin/providers", {
          // Absent lets the control plane derive host:port, which is what the
          // card has always been labelled with.
          name: draft.name.trim() || undefined,
          kind: cloud ? "cloud" : undefined,
          catalogue_key: cloud ? draft.catalogue_key : undefined,
          api_base: draft.api_base.trim(),
          // A catalogue entry carries its vendor's protocol and header; only
          // a typed endpoint has to be told.
          protocol: cloud ? undefined : draft.protocol,
          upstream_api_key: draft.upstream_api_key.trim() || undefined,
          credential_kind:
            draft.credential_kind === "static"
              ? undefined
              : draft.credential_kind,
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setDraft(null);
      reload();
    }
  };

  const rotate = async (id) => {
    const ok = await attempt(
      () => api.patch(`/admin/providers/${id}`, { upstream_api_key: key }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setRotating(null);
      setKey("");
      reload();
    }
  };

  const rename = async (id) => {
    if (!newName.trim()) return;
    const ok = await attempt(
      () => api.patch(`/admin/providers/${id}`, { name: newName.trim() }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setRenaming(null);
      setNewName("");
      reload();
    }
  };

  const save = async (id) => {
    const ok = await attempt(
      () =>
        api.patch(`/admin/providers/${id}`, {
          name: edit.name?.trim() || undefined,
          api_base: edit.api_base?.trim() || undefined,
          protocol: edit.protocol || undefined,
          auth_header: edit.auth_header?.trim() || undefined,
          // "" is meaningful here: it clears the scheme, which is how a raw
          // key is sent. Only an untouched field is omitted.
          auth_scheme: edit.auth_scheme,
          kind: edit.kind || undefined,
          // Absent leaves the stored credential alone — the form cannot read
          // it back, so it must not send an empty one and wipe it.
          upstream_api_key: edit.upstream_api_key || undefined,
          skip_validation: edit.skip_validation || undefined,
        }),
      setError,
      onUnauthorised,
    );
    if (ok) {
      setEditing(null);
      setEdit({});
      reload();
    }
  };

  const remove = async (g) => {
    // The API refuses while models remain rather than cascading, so the
    // confirmation only has to cover the case it will actually allow.
    if (!window.confirm(`Delete provider ${g.host}?`)) return;
    const ok = await attempt(
      () => api.del(`/admin/providers/${g.id}`),
      setError,
      onUnauthorised,
    );
    if (ok) reload();
  };

  // One card per provider row. The models query still supplies the names and
  // the health lookup, both of which hang off a model rather than a provider.
  const groups = new Map();
  for (const p of data.providers) {
    groups.set(p.id, {
      id: p.id,
      origin: p.api_base,
      host: p.name,
      kind: p.kind,
      node: p.node,
      protocol: p.protocol,
      auth_header: p.auth_header,
      bases: new Set([p.api_base]),
      models: new Set(),
      protocols: new Set([p.protocol]),
      credentialled: p.has_upstream_api_key ? 1 : 0,
      backends: p.model_count,
      up: 0,
      reported: 0,
    });
  }
  for (const m of data.models) {
    const g = groups.get(m.provider_id);
    if (!g) continue;
    g.models.add(m.name);
    for (const b of m.backends) {
      const h = health.get(backendKey(b.api_base, b.upstream_model || m.name));
      if (h) {
        g.reported += 1;
        if (h.healthy) g.up += 1;
      }
    }
  }

  const all = [...groups.values()].sort((a, b) => a.host.localeCompare(b.host));
  const shown = all
    .filter((g) => {
      if (filter === "Hosted") return g.kind === "cloud";
      if (filter === "Self-hosted") return g.kind !== "cloud";
      if (filter === "Native protocol")
        return [...g.protocols].some((p) => p !== "openai");
      return true;
    })
    .filter(
      (g) => !search || g.host.toLowerCase().includes(search.toLowerCase()),
    );

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      <Row gap={10}>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 8,
            background: "var(--panel)",
            border: "1px solid var(--line)",
            borderRadius: 8,
            padding: "8px 12px",
            width: 300,
          }}
        >
          <Mono style={{ color: "var(--fg-5)", fontSize: 12 }}>/</Mono>
          <input
            placeholder="filter by host"
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
        {FILTERS.map((f) => (
          <Chip key={f} active={filter === f} onClick={() => setFilter(f)}>
            {f}
          </Chip>
        ))}
        <Spacer />
        <Button
          variant="primary"
          onClick={() => {
            setCatFilter("");
            setDraft(draft ? null : { ...BLANK });
          }}
        >
          {draft ? "Cancel" : "Add provider"}
        </Button>
      </Row>

      {draft && (
        <Card title="Add a provider">
          <form onSubmit={create}>
            <Stack gap={12}>
              <Row gap={8}>
                {/* The two ways in differ only in where the address comes
                    from: a catalogue entry knows the vendor's URL and which
                    header it wants its key in, a typed one does not. */}
                <Chip
                  type="button"
                  active={draft.mode === "cloud"}
                  onClick={() =>
                    setDraft({ ...BLANK, mode: "cloud", name: draft.name })
                  }
                >
                  Cloud provider
                </Chip>
                <Chip
                  type="button"
                  active={draft.mode === "custom"}
                  onClick={() =>
                    setDraft({ ...BLANK, mode: "custom", name: draft.name })
                  }
                >
                  Custom endpoint
                </Chip>
              </Row>

              {draft.mode === "cloud" && (
                <Field
                  label="Provider"
                  hint={
                    catFilter.trim()
                      ? `${catalogue.length} of ${(data.catalogue || []).length} match. Not a limit — anything speaking the OpenAI API works whether or not it is listed.`
                      : "Fills in the address and the header this vendor wants its key in. Not a limit — anything speaking the OpenAI API works whether or not it is listed."
                  }
                >
                  <input
                    placeholder="filter…"
                    value={catFilter}
                    onChange={(e) => setCatFilter(e.target.value)}
                    style={{ marginBottom: 6 }}
                  />
                  <select
                    value={draft.catalogue_key}
                    onChange={(e) => {
                      const c = (data.catalogue || []).find(
                        (x) => x.key === e.target.value,
                      );
                      setDraft({
                        ...draft,
                        catalogue_key: e.target.value,
                        api_base: c ? c.base_url : "",
                        protocol: c ? c.protocol : "openai",
                        // Reset with the entry, or switching away from Vertex
                        // would leave a service-account kind selected behind a
                        // control that is no longer on screen — and the save
                        // would fail on a key that is not a key file.
                        credential_kind: c ? c.credential_kinds[0] : "static",
                      });
                    }}
                  >
                    <option value="">choose…</option>
                    {catalogue.map((c) => (
                      <option key={c.key} value={c.key}>
                        {c.display_name}
                      </option>
                    ))}
                  </select>
                </Field>
              )}

              <Row gap={10} style={{ alignItems: "flex-start" }}>
                <Field
                  label="API base"
                  style={{ flex: 2 }}
                  hint={
                    draft.api_base.includes("<")
                      ? "This address still has a placeholder in it — fill it in, or it will resolve nowhere."
                      : undefined
                  }
                >
                  <input
                    placeholder="https://host:port/v1"
                    value={draft.api_base}
                    onChange={(e) =>
                      setDraft({ ...draft, api_base: e.target.value })
                    }
                  />
                </Field>
                <Field
                  label="Name"
                  style={{ flex: 1 }}
                  hint="Optional — what the card will be called."
                >
                  <input
                    placeholder={derivedName(draft.api_base) || "host:port"}
                    value={draft.name}
                    onChange={(e) =>
                      setDraft({ ...draft, name: e.target.value })
                    }
                  />
                </Field>
                {draft.mode === "custom" && (
                  <Field label="Protocol" style={{ flex: 0.8 }}>
                    <select
                      value={draft.protocol}
                      onChange={(e) =>
                        setDraft({ ...draft, protocol: e.target.value })
                      }
                    >
                      <option value="openai">openai</option>
                      <option value="anthropic">anthropic</option>
                      <option value="gemini">gemini</option>
                    </select>
                  </Field>
                )}
              </Row>

              <Row gap={10} style={{ alignItems: "flex-start" }}>
                <Field
                  label="Credential"
                  style={{ flex: 2 }}
                  hint={
                    entry?.auth_header
                      ? `Sent as ${entry.auth_header}. Encrypted at rest and never readable back.`
                      : "Encrypted at rest and never readable back through this API."
                  }
                >
                  <input
                    type="password"
                    placeholder={
                      draft.credential_kind === "gcp_service_account"
                        ? "service-account JSON key file"
                        : "api key"
                    }
                    value={draft.upstream_api_key}
                    onChange={(e) =>
                      setDraft({ ...draft, upstream_api_key: e.target.value })
                    }
                  />
                </Field>
                {/* Asked only where there is something to answer. Thirteen of
                    the fourteen catalogue entries take a static key and so
                    does every self-hosted endpoint; putting a Google-shaped
                    question on the form for someone adding Groq is noise. The
                    catalogue says which kinds an entry accepts (migration
                    0040) rather than this file naming a vendor. A typed
                    address gets the choice because Vertex can be reached that
                    way too. */}
                {kinds.length > 1 && (
                  <Field
                    label="Credential kind"
                    style={{ flex: 1 }}
                    hint="Vertex AI mints a token from a service-account key file; it cannot use a static secret."
                  >
                    <select
                      value={draft.credential_kind}
                      onChange={(e) =>
                        setDraft({ ...draft, credential_kind: e.target.value })
                      }
                    >
                      {kinds.map((k) => (
                        <option key={k} value={k}>
                          {k}
                        </option>
                      ))}
                    </select>
                  </Field>
                )}
              </Row>

              {entry?.notes && <Muted>{entry.notes}</Muted>}

              <Row>
                <Muted>
                  A provider carries the endpoint and its credential. Which of
                  its models to serve is decided on the{" "}
                  <a href="#/models" onClick={() => go("models")}>
                    Provider models
                  </a>{" "}
                  screen, so nothing is registered by adding one.
                </Muted>
                <Spacer />
                <Button type="submit" variant="primary">
                  Add provider
                </Button>
              </Row>
            </Stack>
          </form>
        </Card>
      )}

      <Muted>
        A provider is a row in a table, not a code change — anything speaking
        the OpenAI API is already supported. A model is attached to a provider
        on the{" "}
        <a href="#/models" onClick={() => go("models")}>
          Provider models
        </a>{" "}
        screen.
      </Muted>

      {shown.length === 0 ? (
        <Card>
          <Empty>
            {all.length === 0
              ? "No providers are configured yet."
              : "No provider matches that filter."}
          </Empty>
        </Card>
      ) : (
        <Grid cols={3}>
          {shown.map((g) => {
            const native = [...g.protocols].filter((p) => p !== "openai");
            return (
              <Card key={g.id}>
                <Stack gap={12}>
                  <Row style={{ flexWrap: "nowrap", alignItems: "flex-start" }}>
                    <Row gap={10} style={{ flexWrap: "nowrap", minWidth: 0 }}>
                      <div style={{ minWidth: 0 }}>
                        {renaming === g.id ? (
                          <input
                            autoFocus
                            value={newName}
                            style={{ font: "600 13px/1.2 var(--sans)" }}
                            onChange={(e) => setNewName(e.target.value)}
                            onKeyDown={(e) => {
                              if (e.key === "Enter") rename(g.id);
                              if (e.key === "Escape") setRenaming(null);
                            }}
                          />
                        ) : (
                          <Ellipsis
                            style={{
                              font: "600 13px/1.2 var(--sans)",
                              // Only where a rename sticks. A dynamic
                              // provider's name comes from the agent on every
                              // heartbeat, so editing it here would be undone
                              // within the beat and look like the UI losing
                              // the change.
                              cursor: g.kind === "dynamic" ? "default" : "text",
                            }}
                            title={
                              g.kind === "dynamic"
                                ? `${g.host} — named by the agent on ${g.node || "its host"}`
                                : `${g.host} — click to rename`
                            }
                            onClick={() => {
                              if (g.kind === "dynamic") return;
                              setRenaming(g.id);
                              setNewName(g.host);
                            }}
                          >
                            {g.host}
                          </Ellipsis>
                        )}
                        <div
                          style={{
                            font: "400 10px/1.4 var(--sans)",
                            color: "var(--fg-4)",
                          }}
                        >
                          {fmtInt(g.backends)} model
                          {g.backends === 1 ? "" : "s"}
                          {g.node ? ` · registered by ${g.node}` : ""}
                        </div>
                      </div>
                    </Row>
                    <div style={{ flex: 1 }} />
                    <Row gap={6} style={{ flexWrap: "nowrap" }}>
                      {/* What kind of thing this is, which decides whether
                          anything may remove it: only `dynamic` is swept, and
                          the other two are here because a human said so. */}
                      <Pill tone={KIND_TONE[g.kind] || "neutral"} mono>
                        {g.kind}
                      </Pill>
                      <Pill tone={native.length ? "violet" : "neutral"} mono>
                        {native.length ? native.join(" · ") : "openai"}
                      </Pill>
                    </Row>
                  </Row>

                  {[...g.bases].slice(0, 2).map((base) => (
                    <Ellipsis
                      key={base}
                      title={base}
                      style={{
                        font: "400 11px/1.4 var(--mono)",
                        color: "var(--fg-3)",
                        background: "var(--panel-2)",
                        border: "1px solid var(--line-mid)",
                        borderRadius: 7,
                        padding: "8px 10px",
                      }}
                    >
                      {base}
                    </Ellipsis>
                  ))}
                  {g.bases.size > 2 && (
                    <Muted>and {g.bases.size - 2} more paths</Muted>
                  )}

                  <Row
                    style={{
                      paddingTop: 10,
                      borderTop: "1px solid var(--line-mid)",
                      flexWrap: "nowrap",
                    }}
                  >
                    <Row gap={6} style={{ flexWrap: "nowrap" }}>
                      <Dot tone={g.credentialled > 0 ? "ok" : "muted"} />
                      <Muted>
                        {/* One credential per provider, however many models
                            ride on it — that is the point of the split, so
                            "1 of 3 credentialled" would read as a shortfall
                            where there is none. */}
                        {g.credentialled === 0
                          ? "no credential"
                          : "credential set"}
                      </Muted>
                    </Row>
                    <div style={{ flex: 1 }} />
                    {g.reported === 0 ? (
                      <Muted>not yet probed</Muted>
                    ) : (
                      <Pill
                        tone={
                          g.up === g.reported
                            ? "ok"
                            : g.up === 0
                              ? "bad"
                              : "warn"
                        }
                      >
                        {g.up} of {g.reported} up
                      </Pill>
                    )}
                  </Row>

                  {editing === g.id ? (
                    <Stack gap={8}>
                      <Field
                        label="Name"
                        hint={
                          g.kind === "dynamic"
                            ? `Named by the agent on ${g.node || "its host"}; it will be set back on the next heartbeat.`
                            : undefined
                        }
                      >
                        <input
                          value={edit.name ?? ""}
                          onChange={(e) =>
                            setEdit({ ...edit, name: e.target.value })
                          }
                        />
                      </Field>
                      <Field label="API base">
                        <input
                          value={edit.api_base ?? ""}
                          onChange={(e) =>
                            setEdit({ ...edit, api_base: e.target.value })
                          }
                        />
                      </Field>
                      <Row gap={8} style={{ alignItems: "flex-start" }}>
                        <Field label="Protocol" style={{ flex: 1 }}>
                          <select
                            value={edit.protocol ?? "openai"}
                            onChange={(e) =>
                              setEdit({ ...edit, protocol: e.target.value })
                            }
                          >
                            <option value="openai">openai</option>
                            <option value="anthropic">anthropic</option>
                            <option value="gemini">gemini</option>
                          </select>
                        </Field>
                        {/* Handing an endpoint to the agent on its host, or
                            taking it back. Only `dynamic` is ever removed
                            automatically, which is what makes this a
                            deliberate act rather than a label. */}
                        <Field label="Kind" style={{ flex: 1 }}>
                          <select
                            value={edit.kind ?? "static"}
                            onChange={(e) =>
                              setEdit({ ...edit, kind: e.target.value })
                            }
                          >
                            <option value="static">static</option>
                            <option value="cloud">cloud</option>
                            <option value="dynamic">dynamic</option>
                          </select>
                        </Field>
                      </Row>
                      <Row gap={8} style={{ alignItems: "flex-start" }}>
                        <Field label="Auth header" style={{ flex: 1 }}>
                          <input
                            value={edit.auth_header ?? ""}
                            onChange={(e) =>
                              setEdit({ ...edit, auth_header: e.target.value })
                            }
                          />
                        </Field>
                        <Field
                          label="Auth scheme"
                          style={{ flex: 1 }}
                          hint="Empty sends the key raw."
                        >
                          <input
                            value={edit.auth_scheme ?? ""}
                            onChange={(e) =>
                              setEdit({ ...edit, auth_scheme: e.target.value })
                            }
                          />
                        </Field>
                      </Row>
                      <Field
                        label="Credential"
                        hint="Leave empty to keep the one already stored — this form cannot read it back."
                      >
                        <input
                          type="password"
                          placeholder={
                            g.credentialled ? "unchanged" : "no key set"
                          }
                          value={edit.upstream_api_key ?? ""}
                          onChange={(e) =>
                            setEdit({
                              ...edit,
                              upstream_api_key: e.target.value,
                            })
                          }
                        />
                      </Field>
                      <label
                        style={{
                          display: "flex",
                          gap: 8,
                          alignItems: "center",
                        }}
                      >
                        <input
                          type="checkbox"
                          checked={!!edit.skip_validation}
                          onChange={(e) =>
                            setEdit({
                              ...edit,
                              skip_validation: e.target.checked,
                            })
                          }
                        />
                        <Muted>
                          Save without dialling it first. Only needed when a
                          provider rejects a key you know is right.
                        </Muted>
                      </label>
                      <Row gap={8} style={{ flexWrap: "nowrap" }}>
                        <Spacer />
                        <Button
                          variant="small"
                          onClick={() => {
                            setEditing(null);
                            setEdit({});
                          }}
                        >
                          cancel
                        </Button>
                        <Button variant="primary" onClick={() => save(g.id)}>
                          Save
                        </Button>
                      </Row>
                    </Stack>
                  ) : renaming === g.id ? (
                    <Row gap={8} style={{ flexWrap: "nowrap" }}>
                      <Muted>Enter to save, Escape to cancel.</Muted>
                      <Spacer />
                      <Button variant="small" onClick={() => setRenaming(null)}>
                        cancel
                      </Button>
                      <Button variant="primary" onClick={() => rename(g.id)}>
                        Rename
                      </Button>
                    </Row>
                  ) : rotating === g.id ? (
                    <Row gap={8} style={{ flexWrap: "nowrap" }}>
                      <input
                        type="password"
                        placeholder="new api key"
                        style={{ flex: 1 }}
                        value={key}
                        onChange={(e) => setKey(e.target.value)}
                      />
                      <Button
                        variant="small"
                        onClick={() => {
                          setRotating(null);
                          setKey("");
                        }}
                      >
                        cancel
                      </Button>
                      <Button variant="primary" onClick={() => rotate(g.id)}>
                        Save
                      </Button>
                    </Row>
                  ) : (
                    <Row gap={8}>
                      <Button
                        variant="small"
                        onClick={() => {
                          setEditing(g.id);
                          setEdit({
                            name: g.host,
                            api_base: g.origin,
                            protocol: [...g.protocols][0] || "openai",
                            auth_header: g.auth_header || "authorization",
                            auth_scheme: g.auth_scheme ?? "",
                            kind: g.kind,
                          });
                        }}
                      >
                        edit
                      </Button>
                      <Button
                        variant="small"
                        onClick={() => {
                          setRotating(g.id);
                          setKey("");
                        }}
                      >
                        {/* One credential per provider, so this is one write
                            however many models ride on it — the reason the
                            key lives here and not on a model. */}
                        {g.credentialled ? "rotate key" : "set key"}
                      </Button>
                      <Spacer />
                      <Button
                        variant="smallDanger"
                        onClick={() => remove(g)}
                        disabled={g.backends > 0}
                        title={
                          g.backends > 0
                            ? "Delete its models first — removing the provider would take their routing targets with them"
                            : undefined
                        }
                      >
                        delete
                      </Button>
                    </Row>
                  )}
                </Stack>
              </Card>
            );
          })}
        </Grid>
      )}
    </Stack>
  );
}
