-- Append-only audit log for token usage, per the design's Snapshot protocol
-- (`POST /usage`, the reverse channel: batched, fire-and-forget, dropped on
-- failure). `budgets.tokens_used` (P3) will be the running counter the
-- snapshot carries, reconciled from these rows; nothing reads this table yet.
--
-- Both foreign keys are ON DELETE CASCADE, matching `api_keys.principal_id`
-- and `model_backends.model_id` elsewhere in this schema: a usage row for a
-- principal or model that no longer exists describes nothing an operator can
-- act on, so it is not worth keeping around as an orphan. This is also why
-- `POST /usage` (src/control/api.rs) silently drops batch rows that name a
-- principal or model id which does not (or no longer) exist, rather than
-- failing the whole batch — the same judgement call this cascade makes,
-- applied at ingest time instead of at deletion time.
CREATE TABLE usage_events (
    id                 BIGSERIAL PRIMARY KEY,
    principal_id       BIGINT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    model_id           BIGINT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    prompt_tokens      BIGINT NOT NULL,
    completion_tokens  BIGINT NOT NULL,
    at                 TIMESTAMPTZ NOT NULL
);
CREATE INDEX usage_events_principal ON usage_events(principal_id);
CREATE INDEX usage_events_model ON usage_events(model_id);
CREATE INDEX usage_events_at ON usage_events(at);
