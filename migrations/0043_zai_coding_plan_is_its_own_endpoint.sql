-- Z.ai's coding plan is a different address, so it is a different entry.
--
-- Same vendor, same key format, different base URL:
-- `/api/coding/paas/v4` rather than `/api/paas/v4`. A catalogue entry is an
-- address and the auth that reaches it, so one entry cannot describe both --
-- and a provider is its endpoint, so subscribing to the coding plan means a
-- second provider row rather than editing the first.
--
-- Listed separately rather than left as a note on the existing entry because
-- the point of the catalogue is that an operator does not have to know this.

INSERT INTO provider_catalogue (key, display_name, base_url, protocol, auth_header, auth_scheme, notes) VALUES
  ('zai_coding', 'Z.ai (coding plan)', 'https://api.z.ai/api/coding/paas/v4', 'openai', 'authorization', 'Bearer',
   'The coding-plan endpoint; the general one is Z.ai')
ON CONFLICT (key) DO NOTHING;
