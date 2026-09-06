import React, { useState } from "react";
import { api } from "../api.js";
import { attempt, useLoader } from "../load.js";
import {
  Button,
  Card,
  ErrorNote,
  Empty,
  Field,
  Loading,
  Mono,
  Muted,
  Pill,
  Row,
  Spacer,
  Stack,
  Table,
  Tr,
  fmtWhen,
} from "../ui.jsx";

const COLS = [
  { label: "NAME", width: "1.1fr" },
  { label: "PRINCIPAL", width: "1fr" },
  { label: "PREFIX", width: "1.1fr" },
  { label: "EXPIRES", width: ".9fr" },
  { label: "STATUS", width: ".7fr" },
  { label: "LAST USED", width: ".9fr" },
  { label: "", width: "80px", align: "right" },
];

const EXPIRY_CHOICES = [
  ["", "never"],
  ["30", "30 days"],
  ["90", "90 days"],
  ["365", "1 year"],
];

export function Keys({ onUnauthorised }) {
  const [draft, setDraft] = useState({
    name: "",
    principal_id: "",
    days: "90",
  });
  const [created, setCreated] = useState(null);
  const [copied, setCopied] = useState(false);

  const { data, error, loading, reload, setError } = useLoader(
    async () => {
      const [keys, principals] = await Promise.all([
        api.get("/admin/keys"),
        api.get("/admin/principals"),
      ]);
      return { keys, principals };
    },
    { onUnauthorised },
  );

  if (loading && !data) return <Loading />;
  if (!data)
    return <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>;

  const create = async (e) => {
    e.preventDefault();
    if (!draft.name.trim() || !draft.principal_id) return;
    const expires_at = draft.days
      ? new Date(Date.now() + Number(draft.days) * 86400_000).toISOString()
      : undefined;
    try {
      const resp = await api.post("/admin/keys", {
        name: draft.name.trim(),
        principal_id: draft.principal_id,
        expires_at,
      });
      // Shown once and never retrievable again — this UI does not try, and
      // does not store it anywhere it could be read back from.
      setCreated(resp);
      setCopied(false);
      setDraft({ ...draft, name: "" });
      setError(null);
      reload();
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Stack>
      <ErrorNote onDismiss={() => setError(null)}>{error}</ErrorNote>

      {created && (
        <Card
          tone="accent"
          style={{ background: "#0e1a15", borderColor: "var(--ok-line)" }}
        >
          <div
            style={{
              font: "600 12px var(--sans)",
              color: "var(--ok-fg)",
              marginBottom: 8,
            }}
          >
            Key created — copy it now, it is never shown again
          </div>
          <Row gap={8} style={{ flexWrap: "nowrap" }}>
            <div
              style={{
                flex: 1,
                font: "400 12px var(--mono)",
                background: "var(--bg)",
                border: "1px solid var(--ok-line)",
                borderRadius: 8,
                padding: "10px 12px",
                wordBreak: "break-all",
                minWidth: 0,
              }}
            >
              {created.key}
            </div>
            <Button
              variant="primary"
              style={{ background: "var(--ok)", color: "#06110c" }}
              onClick={async () => {
                try {
                  await navigator.clipboard.writeText(created.key);
                  setCopied(true);
                } catch {
                  // Clipboard access can be refused (an insecure origin, a
                  // permissions policy). Say so rather than showing "Copied"
                  // for a copy that did not happen — the key is on screen and
                  // selectable either way.
                  setError(
                    "The browser refused clipboard access — select and copy manually.",
                  );
                }
              }}
            >
              {copied ? "Copied" : "Copy"}
            </Button>
            <Button onClick={() => setCreated(null)}>Done</Button>
          </Row>
        </Card>
      )}

      <Card title="New key">
        <form onSubmit={create}>
          <Row gap={10} style={{ flexWrap: "nowrap", alignItems: "flex-end" }}>
            <Field label="KEY NAME" style={{ flex: 1 }}>
              <input
                placeholder="ci-runner"
                value={draft.name}
                onChange={(e) => setDraft({ ...draft, name: e.target.value })}
              />
            </Field>
            <Field label="PRINCIPAL" style={{ flex: 1 }}>
              <select
                value={draft.principal_id}
                onChange={(e) =>
                  setDraft({ ...draft, principal_id: e.target.value })
                }
              >
                <option value="">principal…</option>
                {data.principals.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="EXPIRES" style={{ width: 150 }}>
              <select
                value={draft.days}
                onChange={(e) => setDraft({ ...draft, days: e.target.value })}
              >
                {EXPIRY_CHOICES.map(([v, l]) => (
                  <option key={l} value={v}>
                    {l}
                  </option>
                ))}
              </select>
            </Field>
            <Button variant="primary" type="submit">
              Create key
            </Button>
          </Row>
        </form>
      </Card>

      <Card>
        {data.keys.length === 0 ? (
          <Empty>No keys yet.</Empty>
        ) : (
          <Table cols={COLS}>
            {data.keys.map((k) => {
              const expired =
                k.expires_at && new Date(k.expires_at) < new Date();
              return (
                <Tr
                  key={k.id}
                  cols={COLS}
                  cells={[
                    <span key="n" style={{ fontWeight: 500 }}>
                      {k.name}
                    </span>,
                    <Mono key="p" style={{ color: "var(--fg-3)" }}>
                      {k.principal}
                    </Mono>,
                    <Mono key="x" style={{ color: "var(--fg-2)" }}>
                      {k.prefix}
                    </Mono>,
                    <span key="e" style={{ color: "var(--fg-3)" }}>
                      {k.expires_at
                        ? new Date(k.expires_at).toLocaleDateString()
                        : "never"}
                    </span>,
                    <Pill
                      key="s"
                      tone={k.disabled ? "bad" : expired ? "warn" : "ok"}
                    >
                      {k.disabled ? "revoked" : expired ? "expired" : "active"}
                    </Pill>,
                    <Mono
                      key="u"
                      style={{ color: "var(--fg-3)", fontSize: 11 }}
                    >
                      {fmtWhen(k.last_used_at)}
                    </Mono>,
                    !k.disabled ? (
                      <Button
                        key="r"
                        variant="smallDanger"
                        onClick={async () => {
                          if (
                            !window.confirm(
                              `Revoke ${k.name}? Callers using it fail immediately.`,
                            )
                          )
                            return;
                          const ok = await attempt(
                            () => api.del(`/admin/keys/${k.id}`),
                            setError,
                            onUnauthorised,
                          );
                          if (ok) reload();
                        }}
                      >
                        revoke
                      </Button>
                    ) : (
                      <span key="r" />
                    ),
                  ]}
                />
              );
            })}
          </Table>
        )}
      </Card>

      <Row>
        <Muted>
          Keys are hashed with SHA-256 and only the prefix is stored in the
          clear; passwords use Argon2id. The two are deliberately different — a
          key is high-entropy random, a password is low-entropy and
          human-chosen.
        </Muted>
        <Spacer />
        <Muted>
          Revocation reaches a running proxy within one snapshot poll, not
          instantly.
        </Muted>
      </Row>
    </Stack>
  );
}
