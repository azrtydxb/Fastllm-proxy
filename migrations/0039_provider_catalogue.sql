-- The provider catalogue becomes something you can pick from.
--
-- `docs/providers.md` has always listed what works, and `tests/doc_claims.rs`
-- has always counted it. But it was prose: adding a cloud provider meant
-- reading the page, copying a base URL by hand, and knowing which header that
-- vendor wants its key in. The list existed so you did not have to go and find
-- the URL, and then you had to go and find the URL.
--
-- What is seeded here is the entries the page actually documents an endpoint
-- for. The page names about a hundred providers and gives a host for
-- thirty-odd of them; the rest are counted rather than specified. Seeding the
-- others would mean inventing their base URLs, and a catalogue that confidently
-- prefills a wrong endpoint is worse than one that admits it does not know —
-- so anything not here stays a typed address, which has always worked and
-- still does.

CREATE TABLE provider_catalogue (
    -- Stable handle, referenced by `providers.catalogue_key`.
    key          TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    -- What goes in `providers.api_base`. May contain `<placeholders>` a human
    -- must fill in — Bedrock and Vertex both encode a region, and pretending
    -- otherwise would prefill an address that cannot resolve.
    base_url     TEXT NOT NULL,
    protocol     TEXT NOT NULL DEFAULT 'openai'
                 CHECK (protocol IN ('openai', 'anthropic', 'gemini')),
    -- Gemini wants `x-goog-api-key`, Anthropic `x-api-key`, and both send the
    -- key raw rather than behind a scheme. Carried here so choosing the entry
    -- fills them in, which is the whole point.
    auth_header  TEXT NOT NULL DEFAULT 'authorization',
    auth_scheme  TEXT NULL DEFAULT 'Bearer',
    notes        TEXT
);

COMMENT ON TABLE provider_catalogue IS
    'Known providers and how to reach them. Not a permission list and not a '
    'limit: anything speaking the OpenAI API works whether or not it is here.';

INSERT INTO provider_catalogue (key, display_name, base_url, protocol, auth_header, auth_scheme, notes) VALUES
  ('openrouter',  'OpenRouter',       'https://openrouter.ai/api/v1',        'openai',    'authorization', 'Bearer', 'Fronts ~400 models behind one key'),
  ('openai',      'OpenAI',           'https://api.openai.com/v1',           'openai',    'authorization', 'Bearer', NULL),
  ('groq',        'Groq',             'https://api.groq.com/openai/v1',      'openai',    'authorization', 'Bearer', NULL),
  ('deepseek',    'DeepSeek',         'https://api.deepseek.com/v1',         'openai',    'authorization', 'Bearer', NULL),
  ('xai',         'xAI',              'https://api.x.ai/v1',                 'openai',    'authorization', 'Bearer', NULL),
  ('mistral',     'Mistral',          'https://api.mistral.ai/v1',           'openai',    'authorization', 'Bearer', NULL),
  ('perplexity',  'Perplexity',       'https://api.perplexity.ai',           'openai',    'authorization', 'Bearer', NULL),
  ('cerebras',    'Cerebras',         'https://api.cerebras.ai/v1',          'openai',    'authorization', 'Bearer', NULL),
  ('sambanova',   'SambaNova',        'https://api.sambanova.ai/v1',         'openai',    'authorization', 'Bearer', NULL),
  ('cohere',      'Cohere',           'https://api.cohere.ai/compatibility/v1', 'openai', 'authorization', 'Bearer', NULL),
  ('bedrock',     'Amazon Bedrock',   'https://bedrock-runtime.<region>.amazonaws.com/openai/v1', 'openai', 'authorization', 'Bearer', 'Replace <region>'),
  ('vertex',      'Google Vertex AI', 'https://<region>-aiplatform.googleapis.com/v1/projects/<project>/locations/<region>/endpoints/openapi', 'openai', 'authorization', 'Bearer', 'Replace <region> and <project>; supports a service-account credential'),
  ('anthropic',   'Anthropic',        'https://api.anthropic.com/v1',        'anthropic', 'x-api-key',     NULL,     'Messages API, translated; SSE re-framed to OpenAI chunks'),
  ('gemini',      'Gemini',           'https://generativelanguage.googleapis.com/v1beta', 'gemini', 'x-goog-api-key', NULL, 'generateContent, model in the URL');
