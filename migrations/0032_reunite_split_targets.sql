-- Give back the capacity 0029 quietly took away.
--
-- 0029 split a model with several backends into one provider model per
-- provider and put a frontend model in front of them, so a caller naming the
-- model was unaffected. A frontend model that *already* pointed at it was not
-- so lucky: its target followed the rename onto one of the parts, and the
-- others stopped receiving traffic.
--
-- On the dev cluster `embed` pointed at `bge-m3`, which ran on two hosts. After
-- 0029 it pointed at `bge-m3@192.168.10.246:8890` alone and the second host sat
-- idle. Nothing failed, nothing was logged, and the only symptom was half the
-- throughput -- the kind of thing found weeks later while looking for something
-- else, if at all.
--
-- Forward rather than by amending 0029, which is applied: `sqlx` checksums
-- migrations and refuses to start when an applied one changes.
--
-- One case is deliberately *not* repaired here. `models.is_fallback` is a
-- boolean on one row rather than a list of targets, so a deployment whose
-- fallback model was split keeps the flag on one part and falls back to one
-- provider where it used to have several. Degraded rather than broken, and
-- unfixable at this level: the repair is for the deployment-wide fallback to
-- be able to name a frontend model, which is a change to what a fallback *is*,
-- not something a migration should invent. No such deployment exists to test
-- against here, and shipping untested SQL for one would be worse than saying
-- so.

-- Every sibling a split target is missing. A model 0029 produced is named
-- `<original>@<provider>`, and the frontend model it created carries
-- `<original>` -- a shape nothing else generates, since the admin API cannot
-- rename a model. The generated frontend model is excluded: it already has
-- all of them, and adding them twice would double its weight on one provider.
CREATE TEMP TABLE missing_defaults ON COMMIT DROP AS
SELECT DISTINCT d.virtual_model_id, sibling.id AS model_id, d.weight
FROM virtual_model_defaults d
JOIN models target      ON target.id = d.model_id
JOIN virtual_models gen ON target.name LIKE gen.name || '@%'
JOIN models sibling     ON sibling.name LIKE gen.name || '@%'
                       AND sibling.id <> target.id
WHERE d.virtual_model_id <> gen.id
  AND NOT EXISTS (
        SELECT 1 FROM virtual_model_defaults x
         WHERE x.virtual_model_id = d.virtual_model_id
           AND x.model_id = sibling.id);

-- Appended after whatever the frontend model already has, so an operator's
-- ordering is preserved and the recovered siblings sit behind the target that
-- was already there. Same weight as the target they are siblings of: the model
-- they came from expressed no preference between its backends.
INSERT INTO virtual_model_defaults (virtual_model_id, model_id, weight, position)
SELECT m.virtual_model_id, m.model_id, m.weight,
       COALESCE((SELECT max(position) FROM virtual_model_defaults e
                  WHERE e.virtual_model_id = m.virtual_model_id), -1)
       + ROW_NUMBER() OVER (PARTITION BY m.virtual_model_id ORDER BY m.model_id)
FROM missing_defaults m;

-- The same for a rule's targets, which have the identical problem and the
-- identical fix.
CREATE TEMP TABLE missing_rule_targets ON COMMIT DROP AS
SELECT DISTINCT rt.rule_id, sibling.id AS model_id, rt.weight
FROM rule_targets rt
JOIN routing_rules r    ON r.id = rt.rule_id
JOIN models target      ON target.id = rt.model_id
JOIN virtual_models gen ON target.name LIKE gen.name || '@%'
JOIN models sibling     ON sibling.name LIKE gen.name || '@%'
                       AND sibling.id <> target.id
WHERE r.virtual_model_id <> gen.id
  AND NOT EXISTS (
        SELECT 1 FROM rule_targets x
         WHERE x.rule_id = rt.rule_id AND x.model_id = sibling.id);

INSERT INTO rule_targets (rule_id, model_id, weight, position)
SELECT m.rule_id, m.model_id, m.weight,
       COALESCE((SELECT max(position) FROM rule_targets e WHERE e.rule_id = m.rule_id), -1)
       + ROW_NUMBER() OVER (PARTITION BY m.rule_id ORDER BY m.model_id)
FROM missing_rule_targets m;
