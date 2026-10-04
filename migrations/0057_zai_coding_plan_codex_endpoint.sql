-- The coding plan's OpenAI Responses door, as its own catalogue entry.
--
-- 0043 is the plan's chat-completions endpoint and 0056 its Anthropic one;
-- Codex speaks the Responses API, which Z.ai serves at `/api/v1` — a third
-- address, so a third entry, by the same reasoning as both. The `/models`
-- answer is Codex-shaped (reasoning levels, tool types) because this door
-- exists for Codex; the probe reads it fine, so provider creation needs no
-- carve-out here.

INSERT INTO provider_catalogue (key, display_name, base_url, protocol, auth_header, auth_scheme, notes) VALUES
  ('zai_coding_responses', 'Z.ai (coding plan, Codex)', 'https://api.z.ai/api/v1', 'openai', 'authorization', 'Bearer',
   'The coding plan''s Responses API door; key is the plan''s. Built for Codex — point Codex at the gateway''s /v1/responses with a frontend model whose name matches the upstream id, so the request needs no rewrite')
ON CONFLICT (key) DO NOTHING;
