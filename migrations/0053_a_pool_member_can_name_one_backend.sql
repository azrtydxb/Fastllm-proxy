-- A pool member may name a single backend, not just a model.
--
-- Until now a member was a `provider_model_id`, and since 0045 a model is one
-- row carrying every provider that serves it. So a pool could say "balance
-- across bge-m3" but never "balance across bge-m3 on .245 and .246, and not
-- the third one" — the very thing load balancing is for. The member picker
-- made that visible: filtered to one model's providers there was exactly one
-- row to tick, because the providers were a label on the model, not rows.
--
-- `model_backend_id` NULL keeps the old meaning: the member is the model and
-- brings every provider serving it. Non-NULL means the member is that one
-- attachment. Both live in the same table because they are the same idea at
-- two grains, and a pool may hold either.
ALTER TABLE model_pool_members
    ADD COLUMN model_backend_id UUID REFERENCES model_backends(id) ON DELETE CASCADE;

-- The old rule was "a model at most once per pool", which is exactly what has
-- to stop being true: the same model appears once per backend now. Uniqueness
-- moves to the (model, backend) pair. NULLs do not compare equal in a plain
-- unique index — two model-grain members of the same model would both be
-- allowed — so the NULL is folded to a sentinel to keep "the model itself, at
-- most once" enforced alongside it.
ALTER TABLE model_pool_members
    DROP CONSTRAINT IF EXISTS model_pool_members_pool_id_provider_model_id_key;

CREATE UNIQUE INDEX model_pool_members_pool_model_backend_key
    ON model_pool_members (
        pool_id,
        provider_model_id,
        COALESCE(model_backend_id, '00000000-0000-0000-0000-000000000000'::uuid)
    );

COMMENT ON COLUMN model_pool_members.model_backend_id IS
    'The single attachment this member routes to. NULL means the member is the '
    'model and brings every provider serving it.';
