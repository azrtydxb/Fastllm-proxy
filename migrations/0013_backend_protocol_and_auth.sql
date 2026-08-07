-- Multi-provider support: how a backend is addressed, and in what wire format.
--
-- Every column here defaults to what the code did before it existed — an
-- OpenAI-compatible upstream reached with `Authorization: Bearer` — so this
-- migration changes the behaviour of exactly zero existing rows. That is the
-- point: adding provider support must not be able to alter how the Spark
-- backends already in this table are called.
--
-- `protocol` is constrained rather than free text because an unrecognised
-- value causes the proxy to drop the backend from its snapshot (see
-- `Snapshot::from_wire`). Catching the typo at write time in the control plane
-- is much kinder than discovering it as a silently missing backend later.
ALTER TABLE model_backends
  ADD COLUMN protocol TEXT NOT NULL DEFAULT 'openai'
    CHECK (protocol IN ('openai', 'anthropic', 'gemini')),
  -- Gemini authenticates with `x-goog-api-key`, Anthropic with `x-api-key`,
  -- and both send the key raw rather than behind a scheme — hence a nullable
  -- scheme rather than a boolean, so `Bearer`, a different prefix, and no
  -- prefix at all are all expressible.
  ADD COLUMN auth_header TEXT NOT NULL DEFAULT 'authorization',
  ADD COLUMN auth_scheme TEXT NULL DEFAULT 'Bearer',
  -- Anthropic rejects a request with no `max_tokens`. NULL means "refuse such
  -- a request and say so" rather than "invent a cap": silently truncating
  -- generation at a number no operator chose is the kind of bug that is only
  -- ever found by someone wondering why answers stop mid-sentence.
  ADD COLUMN default_max_tokens INTEGER NULL
    CHECK (default_max_tokens IS NULL OR default_max_tokens > 0);
