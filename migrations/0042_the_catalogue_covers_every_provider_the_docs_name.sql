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
--   * and an account-scoped endpoint encodes a resource, region, workspace or
--     app that only the operator knows (Azure, Databricks, Snowflake,
--     Cloudflare, Heroku).
--
-- Every hosted address here was read off a source that dials it -- LiteLLM's
-- `openai_compatible_endpoints` and provider configs, `go-ai-sdk`, or the
-- vendor's own documentation -- and the `notes` column records which, because
-- `docs/providers.md` warns that vendors move these and the page cannot
-- notice. When one moves, `notes` says where to go and check.
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
  ('lambda',          'Lambda',                'https://api.lambda.ai/v1',                       'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints; api.lambdalabs.com was the older host'),
  ('moonshot',        'Moonshot / Kimi',       'https://api.moonshot.ai/v1',                     'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/moonshot; api.moonshot.cn for the mainland endpoint'),
  ('dashscope',       'Aliyun DashScope',      'https://dashscope.aliyuncs.com/compatible-mode/v1', 'openai', 'authorization', 'Bearer', NULL),
  ('volcengine',      'Volcengine Ark',        'https://ark.cn-beijing.volces.com/api/v3',       'openai', 'authorization', 'Bearer', NULL),
  ('voyage',          'Voyage AI',             'https://api.voyageai.com/v1',                    'openai', 'authorization', 'Bearer', 'Embeddings and rerank'),
  ('jina',            'Jina AI',               'https://api.jina.ai/v1',                         'openai', 'authorization', 'Bearer', 'Embeddings and rerank'),
  ('nvidia_nim',      'NVIDIA NIM',            'https://integrate.api.nvidia.com/v1',            'openai', 'authorization', 'Bearer', NULL),
  ('ai21',            'AI21',                  'https://api.ai21.com/studio/v1',                 'openai', 'authorization', 'Bearer', NULL),
  ('github_models',   'GitHub Models',         'https://models.inference.ai.azure.com',          'openai', 'authorization', 'Bearer', 'A GitHub PAT is the key'),

  -- Hosted. Each `notes` records the source the address came from.
  ('atlascloud',      'AtlasCloud',            'https://api.atlascloud.ai/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: atlascloud.ai docs'),
  ('zai',             'Z.ai',                  'https://api.z.ai/api/paas/v4',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm ZAI_API_BASE'),
  ('bigmodel',        'BigModel',              'https://open.bigmodel.cn/api/paas/v4',                          'openai', 'authorization', 'Bearer', 'Base URL: docs.bigmodel.cn; the mainland endpoint'),
  ('qwen_cloud',      'Qwen Cloud',            'https://dashscope-intl.aliyuncs.com/compatible-mode/v1',                          'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/qwen; the international DashScope endpoint'),
  ('qianfan',         'Baidu Qianfan',         'https://qianfan.baidubce.com/v2',                          'openai', 'authorization', 'Bearer', 'Base URL: Baidu Qianfan docs'),
  ('aihubmix',        'AIHubMix',              'https://aihubmix.com/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: docs.aihubmix.com'),
  ('minimax',         'MiniMax',               'https://api.minimax.io/v1',                          'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/minimax'),
  ('hunyuan',         'Tencent Hunyuan',       'https://api.hunyuan.cloud.tencent.com/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: Tencent Cloud OpenAI-compatible docs'),
  ('sarvam',          'Sarvam',                'https://api.sarvam.ai/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: docs.sarvam.ai'),
  ('baseten',         'Baseten',               'https://inference.baseten.co/v1',                          'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/baseten'),
  ('featherless',     'Featherless',           'https://api.featherless.ai/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints'),
  ('friendliai',      'FriendliAI',            'https://api.friendli.ai/serverless/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm get_llm_provider_logic'),
  ('chutes',          'Chutes',                'https://llm.chutes.ai/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints'),
  ('nscale',          'Nscale',                'https://inference.api.nscale.com/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints'),
  ('gmi_cloud',       'GMI Cloud',             'https://api.gmi-serving.com/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: docs.gmicloud.ai'),
  ('scaleway',        'Scaleway',              'https://api.scaleway.ai/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: Scaleway Generative APIs docs'),
  ('ovhcloud',        'OVHcloud',              'https://oai.endpoints.kepler.ai.cloud.ovh.net/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: OVHcloud AI Endpoints docs'),
  ('vercel_gateway',  'Vercel AI Gateway',     'https://ai-gateway.vercel.sh/v1',                          'openai', 'authorization', 'Bearer', 'Base URL from go-ai-sdk providers/gateway'),
  ('v0',              'v0',                    'https://api.v0.dev/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints'),
  ('poe',             'Poe',                   'https://api.poe.com/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints'),
  ('nanogpt',         'NanoGPT',               'https://nano-gpt.com/api/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints'),
  ('cometapi',        'CometAPI',              'https://api.cometapi.com/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: apidoc.cometapi.com'),
  ('inception',       'Inception',             'https://api.inceptionlabs.ai/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints'),
  ('morph',           'Morph',                 'https://api.morphllm.com/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints'),
  ('clarifai',        'Clarifai',              'https://api.clarifai.com/v2/ext/openai/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm openai_compatible_endpoints'),
  ('wandb',           'Weights & Biases',      'https://api.inference.wandb.ai/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm get_llm_provider_logic'),
  ('gradientai',      'GradientAI',            'https://inference.do-ai.run/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm GRADIENT_AI_SERVERLESS_ENDPOINT'),
  ('anyscale',        'Anyscale',              'https://api.endpoints.anyscale.com/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: litellm get_llm_provider_logic'),
  ('heroku',          'Heroku',                'https://<your-app>.herokuapp.com/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: Heroku has no fixed address: litellm requires HEROKU_API_BASE. Replace <your-app>'),
  ('compactifai',     'CompactifAI',           'https://api.compactif.ai/v1',                          'openai', 'authorization', 'Bearer', 'Base URL: docs.compactif.ai'),
  ('github_copilot',  'GitHub Copilot',        'https://api.githubcopilot.com',                             'openai', 'authorization', 'Bearer', 'Base URL: GitHub Copilot API host'),

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
