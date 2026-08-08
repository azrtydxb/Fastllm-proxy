-- Semantic routing: classes defined by example, not by training.
--
-- A class is a name plus a handful of example prompts. The control plane
-- embeds the examples and averages them into a centroid that ships in the
-- snapshot; the request path embeds the request's last user message and takes
-- the nearest centroid. Both sides embedding bare prompt text is what makes
-- the comparison meaningful — see src/classifier/prompt.rs.
-- There is no training step and no model to fine-tune, so adding a class is a
-- snapshot rebuild exactly like adding a backend.
CREATE TABLE prompt_classes (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',

    -- 'fast' is the static embedding that runs on every classified request
    -- (~115µs). 'refined' is the transformer (~3.3ms), and is only ever
    -- consulted for requests the fast tier landed on a class this one refines.
    tier        TEXT NOT NULL DEFAULT 'fast' CHECK (tier IN ('fast', 'refined')),

    -- How far ahead of the runner-up this class must be before a rule naming
    -- it matches. Per class because measured precision ranges from 98%
    -- (coding) to 35% (extract) across classes an operator might define, and
    -- one threshold cannot serve both. NULL means the deployment default.
    --
    -- Also per *tier*, which is why this lives on the class rather than
    -- globally: the two embedding spaces are shaped differently, and a floor
    -- calibrated on one is meaningless on the other. See docs/classifier.md.
    min_margin  REAL NULL CHECK (min_margin IS NULL OR (min_margin >= 0 AND min_margin <= 2)),

    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Which fast-tier classes a refined class competes with. Only meaningful for
-- tier = 'refined': when the fast tier answers one of these, the request is
-- escalated so the transformer can decide between them.
--
-- This is the gate that keeps the fast path fast. With no rows here — or none
-- belonging to a class any routing rule references — the transformer is never
-- loaded and no request can pay for it.
CREATE TABLE prompt_class_refines (
    class_id    BIGINT NOT NULL REFERENCES prompt_classes(id) ON DELETE CASCADE,
    refines     TEXT NOT NULL,
    PRIMARY KEY (class_id, refines)
);

CREATE TABLE prompt_class_examples (
    id          BIGSERIAL PRIMARY KEY,
    class_id    BIGINT NOT NULL REFERENCES prompt_classes(id) ON DELETE CASCADE,
    prompt      TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX prompt_class_examples_class ON prompt_class_examples (class_id);
