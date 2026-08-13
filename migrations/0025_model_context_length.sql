-- How many tokens a model can actually accept.
--
-- Unmodelled until now, which meant the gateway could route a request to a
-- model that could not possibly serve it and only find out when the upstream
-- returned 400. On this cluster that is not hypothetical: a coding agent
-- routinely sends 90k-token prompts and has sent 214k against a 262k window,
-- so the margin is one large file.
--
-- NULL means unknown, and unknown must not be treated as unlimited *or* as
-- zero. A routing rule that skips a model whose limit it does not know would
-- quietly stop using every model nobody has filled this in for; one that
-- assumes it fits would reintroduce the 400 this exists to prevent. Both
-- readings are wrong in a way that only shows up under load, so the code
-- that consumes this must handle the third state explicitly.
ALTER TABLE models
    ADD COLUMN context_length bigint;

-- Seeded for the models this deployment serves, from each engine's own
-- configuration rather than from a vendor's marketing page:
--   qwen3-6-35b-a3b-nvfp4  --max-model-len 262144 on both DGX nodes
--   bge-m3                 8192, the encoder's maximum sequence length
--   bge-reranker-v2-m3     8192, same
-- Left NULL for the hosted models, whose real limits belong to whoever
-- configures them and change without notice here.
UPDATE models SET context_length = 262144 WHERE name = 'qwen3-6-35b-a3b-nvfp4';
UPDATE models SET context_length = 8192   WHERE name IN ('bge-m3', 'bge-reranker-v2-m3');
