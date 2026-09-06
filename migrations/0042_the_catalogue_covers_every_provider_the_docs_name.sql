-- The catalogue stops being a sample of the list.
--
-- `docs/providers.md` names eighty providers; the catalogue seeded fourteen —
-- the ones the page gave an explicit address for. That made the Add provider
-- dropdown read as the list of what FastLLM supports, when it was the list of
-- what somebody had got round to typing. Everything here has always worked;
-- only the convenience was missing.
--
-- Where the address is known it is filled in. Where it is *not*, the entry is
-- still listed with a `<placeholder>` rather than omitted or guessed at, and
-- `POST /admin/providers` refuses to store an address with one still in it.
-- That is the honest shape for three different reasons:
--
--   * a self-hosted engine has no public address at all -- vLLM, Ollama,
--     llama.cpp, TEI and the rest run whereever the operator put them;
--   * an account-scoped endpoint encodes a resource, region or workspace that
--     only the operator knows (Azure, Databricks, Snowflake, Cloudflare);
--   * and for a handful of hosted vendors the address is one this project has
--     not verified. `docs/providers.md` already warns that vendors move them
--     and the page cannot notice, so a confidently prefilled wrong URL is
--     worse than a box that asks.
--
-- The entry still earns its place in all three cases: it says the provider is
-- supported, and it fills in the protocol and the header that vendor wants its
-- key in, which is the part that is easy to get wrong.

INSERT INTO provider_catalogue (key, display_name, base_url, protocol, auth_header, auth_scheme, notes) VALUES
  -- Hosted, address verified against the vendor's own documentation.
  ('together',        'Together AI',           'https://api.together.xyz/v1',                    'openai', 'authorization', 'Bearer', NULL),
  ('fireworks',       'Fireworks AI',          'https://api.fireworks.ai/inference/v1',          'openai', 'authorization', 'Bearer', NULL),
  ('nebius',          'Nebius AI Studio',      'https://api.studio.nebius.ai/v1',                'openai', 'authorization', 'Bearer', NULL),
  ('deepinfra',       'DeepInfra',             'https://api.deepinfra.com/v1/openai',            'openai', 'authorization', 'Bearer', NULL),
  ('novita',          'Novita AI',             'https://api.novita.ai/v3/openai',                'openai', 'authorization', 'Bearer', NULL),
  ('hyperbolic',      'Hyperbolic',            'https://api.hyperbolic.xyz/v1',                  'openai', 'authorization', 'Bearer', NULL),
  ('lambda',          'Lambda',                'https://api.lambdalabs.com/v1',                  'openai', 'authorization', 'Bearer', NULL),
  ('moonshot',        'Moonshot / Kimi',       'https://api.moonshot.ai/v1',                     'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/moonshot; api.moonshot.cn for the mainland endpoint'),
  ('dashscope',       'Aliyun DashScope',      'https://dashscope.aliyuncs.com/compatible-mode/v1', 'openai', 'authorization', 'Bearer', NULL),
  ('volcengine',      'Volcengine Ark',        'https://ark.cn-beijing.volces.com/api/v3',       'openai', 'authorization', 'Bearer', NULL),
  ('voyage',          'Voyage AI',             'https://api.voyageai.com/v1',                    'openai', 'authorization', 'Bearer', 'Embeddings and rerank'),
  ('jina',            'Jina AI',               'https://api.jina.ai/v1',                         'openai', 'authorization', 'Bearer', 'Embeddings and rerank'),
  ('nvidia_nim',      'NVIDIA NIM',            'https://integrate.api.nvidia.com/v1',            'openai', 'authorization', 'Bearer', NULL),
  ('ai21',            'AI21',                  'https://api.ai21.com/studio/v1',                 'openai', 'authorization', 'Bearer', NULL),
  ('github_models',   'GitHub Models',         'https://models.inference.ai.azure.com',          'openai', 'authorization', 'Bearer', 'A GitHub PAT is the key'),

  -- Hosted, address not verified by this project. Listed so the vendor is
  -- discoverable and the auth shape is prefilled; the address is asked for.
  ('atlascloud',      'AtlasCloud',            'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('zai',             'Z.ai',                  'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('bigmodel',        'BigModel',              'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('qwen_cloud',      'Qwen Cloud',            'https://dashscope-intl.aliyuncs.com/compatible-mode/v1',                          'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/qwen; the international DashScope endpoint'),
  ('qianfan',         'Baidu Qianfan',         'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('aihubmix',        'AIHubMix',              'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('minimax',         'MiniMax',               'https://api.minimax.io/v1',                          'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/minimax'),
  ('hunyuan',         'Tencent Hunyuan',       'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('sarvam',          'Sarvam',                'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('baseten',         'Baseten',               'https://inference.baseten.co/v1',                          'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/baseten'),
  ('featherless',     'Featherless',           'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('friendliai',      'FriendliAI',            'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('chutes',          'Chutes',                'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('nscale',          'Nscale',                'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('gmi_cloud',       'GMI Cloud',             'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('scaleway',        'Scaleway',              'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('ovhcloud',        'OVHcloud',              'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('vercel_gateway',  'Vercel AI Gateway',     'https://ai-gateway.vercel.sh/v1',                          'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/gateway'),
  ('v0',              'v0',                    'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('poe',             'Poe',                   'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('nanogpt',         'NanoGPT',               'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('cometapi',        'CometAPI',              'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('inception',       'Inception',             'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('morph',           'Morph',                 'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('clarifai',        'Clarifai',              'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('wandb',           'Weights & Biases',      'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('gradientai',      'GradientAI',            'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('anyscale',        'Anyscale',              'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('heroku',          'Heroku',                'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('compactifai',     'CompactifAI',           'https://<base-url>/v1',                          'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),
  ('github_copilot',  'GitHub Copilot',        'https://<base-url>',                             'openai', 'authorization', 'Bearer', 'Address not verified here — check the vendor''s docs'),

  -- Account-scoped: the address encodes a resource, region or workspace only
  -- the operator knows.
  ('azure_openai',    'Azure OpenAI',          'https://<resource>.openai.azure.com/openai/deployments/<deployment>', 'openai', 'api-key', NULL, 'Replace <resource> and <deployment>; the key goes in api-key with no Bearer prefix'),
  ('azure_ai',        'Azure AI',              'https://<resource>.services.ai.azure.com/models', 'openai', 'api-key',       NULL,     'Replace <resource>'),
  ('databricks',      'Databricks',            'https://<workspace>/serving-endpoints',          'openai', 'authorization', 'Bearer', 'Replace <workspace> with your workspace host'),
  ('snowflake',       'Snowflake Cortex',      'https://<account>.snowflakecomputing.com/api/v2/cortex/v1', 'openai', 'authorization', 'Bearer', 'Replace <account>'),
  ('cloudflare',      'Cloudflare Workers AI', 'https://api.cloudflare.com/client/v4/accounts/<account>/ai/v1', 'openai', 'authorization', 'Bearer', 'Replace <account>'),

  -- Self-hosted: the address is wherever the operator started it. Every one of
  -- these answers GET /v1/models, which is the whole of what registration and
  -- the sweep need.
  ('vllm',            'vLLM',                  'http://<host>:8000/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted; often needs no credential at all'),
  ('sglang',          'SGLang',                'http://<host>:30000/v1',                         'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('llama_cpp',       'llama.cpp',             'http://<host>:8080/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('ollama',          'Ollama',                'http://<host>:11434/v1',                         'openai', 'authorization', 'Bearer', 'Self-hosted; usually no credential'),
  ('lm_studio',       'LM Studio',             'http://<host>:1234/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('koboldcpp',       'KoboldCpp',             'http://<host>:5001/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('tabbyapi',        'TabbyAPI',              'http://<host>:5000/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('textgen_webui',   'text-generation-webui', 'http://<host>:5000/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('xinference',      'Xinference',            'http://<host>:9997/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('llamafile',       'Llamafile',             'http://<host>:8080/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('docker_model',    'Docker Model Runner',   'http://<host>:12434/engines/v1',                 'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('lemonade',        'Lemonade',              'http://<host>:8000/api/v1',                      'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('huggingface',     'HuggingFace Router',    'https://router.huggingface.co/v1',               'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/huggingface'),
  ('tgi',             'HuggingFace TGI',       'http://<host>:8080/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted'),
  ('infinity',        'Infinity',              'http://<host>:7997/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted; embeddings and rerank'),
  ('tei',             'TEI',                   'http://<host>:8080/v1',                          'openai', 'authorization', 'Bearer', 'Self-hosted; embeddings and rerank')
ON CONFLICT (key) DO NOTHING;
