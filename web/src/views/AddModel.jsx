// Adding a model starts from the provider that serves it.
//
// It used to start from a name: type one, press Create, then find the row you
// just made and fill in an address on it. That order asks the operator to know
// the upstream model's name from memory before anything has offered it, and it
// leaves a model that routes nowhere in between — visible on the screen, in
// the snapshot, and answering nothing.
//
// So: pick a provider, see what it serves, pick one of those. The list is the
// endpoint's own answer to `GET /v1/models`, which is the same call the sweep
// makes, so a name that appears here is one the proxies can actually reach.

import React, { useState } from "react";
import { api } from "../api.js";
import {
  Button,
  Field,
  Modal,
  Muted,
  Row,
  Spacer,
  Stack,
  ErrorNote,
} from "../ui.jsx";

/**
 * A local name for an upstream one.
 *
 * Vendors qualify their models — `openai/gpt-5`, `qwen/qwen3.8-max-0902` — and
 * the qualifier is about where it came from, which the provider already
 * records. Dropping it is right often enough to be worth offering and wrong
 * often enough that it is a filled-in value rather than a silent default: it
 * is visible in the box, and editable, before anything is created.
 */
function suggestedName(upstreamModel) {
  const last = upstreamModel.split("/").pop() || upstreamModel;
  return last.trim();
}

export function AddModel({ providers, onClose, onCreated, onUnauthorised }) {
  const [providerId, setProviderId] = useState("");
  const [served, setServed] = useState(null);
  const [upstream, setUpstream] = useState("");
  const [name, setName] = useState("");
  // Once the operator types a name, it is theirs. Without this, picking a
  // different model would overwrite what they had just written.
  const [named, setNamed] = useState(false);
  const [description, setDescription] = useState("");
  const [maxTokens, setMaxTokens] = useState("");
  // A provider can serve several hundred models — OpenRouter answers with
  // upwards of four hundred — and scrolling a dropdown that long to find one
  // you already know the name of is the slowest way to do it.
  const [filter, setFilter] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);

  const provider = providers.find((p) => String(p.id) === String(providerId));

  const chooseProvider = async (id) => {
    setProviderId(id);
    setUpstream("");
    setServed(null);
    setFilter("");
    if (!id) return;
    setServed({ loading: true });
    try {
      const resp = await api.get(`/admin/providers/${id}/available-models`);
      setServed({ models: resp.models });
    } catch (e) {
      // Kept next to the field rather than raised as a dialog error: a
      // provider that does not implement /v1/models is a fact about that
      // provider, and the name can still be typed.
      setServed({ error: e.message });
    }
  };

  const chooseModel = (m) => {
    setUpstream(m);
    if (!named) setName(suggestedName(m));
  };

  const submit = async (e) => {
    e.preventDefault();
    if (!providerId || !upstream.trim() || !name.trim()) return;
    setBusy(true);
    setError(null);
    let created = null;
    try {
      created = await api.post("/admin/provider-models", {
        name: name.trim(),
        description: description.trim(),
      });
      await api.post(`/admin/provider-models/${created.id}/backends`, {
        provider_id: providerId,
        upstream_model: upstream.trim(),
        default_max_tokens: maxTokens ? Number(maxTokens) : undefined,
      });
      onCreated();
    } catch (err) {
      if (err.status === 401) {
        onUnauthorised?.();
        return;
      }
      // Two writes, one intent. A model left behind by a failed attach would
      // be a name that exists, routes nowhere, and blocks the retry with a
      // duplicate-name conflict — so the half that succeeded is undone.
      if (created) {
        await api.del(`/admin/provider-models/${created.id}`).catch(() => {});
      }
      setError(err.message);
    } finally {
      setBusy(false);
    }
  };

  const list = served?.models;
  // The chosen model always stays in the list even when the filter would drop
  // it: a select whose value is not among its options renders blank, which
  // reads as "your choice was lost" rather than "your filter is narrow".
  const shown = list?.filter(
    (m) =>
      m.upstream_model === upstream ||
      m.upstream_model.toLowerCase().includes(filter.trim().toLowerCase()),
  );

  return (
    <Modal label="Add a model" width={640} onClose={onClose}>
      <form onSubmit={submit}>
        <Stack gap={14}>
          <div style={{ font: "600 14px var(--sans)" }}>Add a model</div>
          <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

          <Field
            label="Provider"
            hint="Where this model runs. Providers are added on the Providers screen."
          >
            <select
              value={providerId}
              onChange={(e) => chooseProvider(e.target.value)}
            >
              <option value="">choose a provider…</option>
              {providers.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name} — {p.api_base}
                </option>
              ))}
            </select>
          </Field>

          {providerId && (
            <Field
              label="Model"
              hint={
                list
                  ? filter.trim()
                    ? `${shown.length} of ${list.length} served by this provider.`
                    : `${list.length} served by this provider, read from it just now.`
                  : served?.loading
                    ? "Asking the provider what it serves…"
                    : undefined
              }
            >
              {list ? (
                <Stack gap={6}>
                  <input
                    placeholder="filter…"
                    value={filter}
                    onChange={(e) => setFilter(e.target.value)}
                  />
                  <select
                    value={upstream}
                    onChange={(e) => chooseModel(e.target.value)}
                    // Tall enough to see the effect of narrowing the filter
                    // without opening the dropdown, which is the whole point
                    // of having one.
                    size={Math.min(Math.max(shown.length, 2), 8)}
                  >
                    <option value="">choose a model…</option>
                    {shown.map((m) => (
                      <option key={m.upstream_model} value={m.upstream_model}>
                        {m.upstream_model}
                        {m.registered ? " · already registered" : ""}
                      </option>
                    ))}
                  </select>
                  {shown.length === 0 && (
                    <Muted>
                      Nothing this provider serves matches {`"${filter}"`}.
                    </Muted>
                  )}
                </Stack>
              ) : (
                <input
                  placeholder="upstream model name"
                  value={upstream}
                  onChange={(e) => chooseModel(e.target.value)}
                  disabled={served?.loading}
                />
              )}
            </Field>
          )}

          {served?.error && (
            <Muted>
              {served.error} — type the upstream model name instead.{" "}
              <a
                href="#"
                onClick={(e) => {
                  e.preventDefault();
                  chooseProvider(providerId);
                }}
              >
                Try again
              </a>
            </Muted>
          )}

          {upstream && (
            <>
              <Row gap={10} style={{ alignItems: "flex-start" }}>
                <Field
                  label="Name"
                  style={{ flex: 1 }}
                  hint="What this is called here. Frontend models are what clients ask for."
                >
                  <input
                    value={name}
                    onChange={(e) => {
                      setNamed(true);
                      setName(e.target.value);
                    }}
                  />
                </Field>
                {/* Anthropic refuses a request that omits max_tokens, so a
                    model on one needs a default. Asked here rather than
                    invented, because inventing it would cap generation at a
                    number nobody chose. */}
                {provider?.protocol === "anthropic" && (
                  <Field
                    label="Default max tokens"
                    style={{ flex: 0.6 }}
                    hint="Anthropic refuses a request with no max_tokens."
                  >
                    <input
                      value={maxTokens}
                      onChange={(e) => setMaxTokens(e.target.value)}
                    />
                  </Field>
                )}
              </Row>
              <Field label="Description" hint="Optional.">
                <input
                  placeholder="what callers should know about it"
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                />
              </Field>
            </>
          )}

          {/* The note on its own line: beside the buttons it wrapped them
              onto two rows in a dialog this narrow, which read as two
              separate actions rather than a choice between them. */}
          <Muted>
            Registering a model does not expose it. A frontend model is what
            clients can ask for.
          </Muted>
          <Row gap={8} style={{ flexWrap: "nowrap" }}>
            <Spacer />
            <Button type="button" onClick={onClose}>
              Cancel
            </Button>
            <Button
              variant="primary"
              type="submit"
              disabled={busy || !providerId || !upstream.trim() || !name.trim()}
            >
              {busy ? "Adding…" : "Add model"}
            </Button>
          </Row>
        </Stack>
      </form>
    </Modal>
  );
}
