-- The coding plan's Anthropic Messages endpoint, as its own catalogue entry.
--
-- 0043 added the coding plan's OpenAI endpoint (`/api/coding/paas/v4`); the
-- plan's Claude Code door is a different protocol at a different path again
-- (`/api/anthropic`), so it is a third entry rather than a note on either —
-- same reasoning as 0043: a catalogue entry is an address and the auth that
-- reaches it, and the general Z.ai entry (`zai`, `/api/paas/v4`) is a
-- different arrangement entirely. Nothing here touches that entry.
--
-- The probe caveat is in the notes because it is load-bearing: this endpoint
-- does not serve `/models`, and answers the reachability probe with HTTP 200
-- wrapping the 404. The provider-creation check knows to accept that shape
-- for this endpoint and nowhere else.

INSERT INTO provider_catalogue (key, display_name, base_url, protocol, auth_header, auth_scheme, notes) VALUES
  ('zai_coding_anthropic', 'Z.ai (coding plan, Claude Code)', 'https://api.z.ai/api/anthropic', 'anthropic', 'authorization', 'Bearer',
   'The coding plan''s Anthropic Messages door; key is the plan''s. Has no /models — the reachability probe accepts that here. Traffic must look like Claude Code, so reach it through the /v1/messages frontend, which passes such clients through natively')
ON CONFLICT (key) DO NOTHING;
