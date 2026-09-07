-- A rule could only ever mean "send it to these targets".
--
-- That leaves things an operator can plainly want and cannot say: "this role
-- may not send 200k-token prompts", "no cloud spend on this key". Those are
-- refusals, and expressing one previously meant routing the request somewhere
-- cheap and hoping, or building the check outside the gateway.
--
-- `route`, `failover`, `balance` and `split` are deliberately NOT four
-- actions. All four mean "order a chain, try the head, fall down the list on
-- failure" and differ only in how the head is chosen, which is what
-- `routing_rules.policy` already says. So there is one routing action, and
-- the enum is for the things that are not routing.
--
-- Every action is terminal. Firewalls have non-terminating rules that mark and
-- continue, and it is tempting -- but "first match wins and the matching rule
-- decides everything" is what lets `POST /admin/routing/dry-run` answer "which
-- rule decided this" with one rule name instead of a trace. Modifiers are
-- therefore fields on a rule that also routes, never separate passes.
ALTER TABLE routing_rules
    ADD COLUMN action text NOT NULL DEFAULT 'route'
        CHECK (action IN ('route', 'deny', 'jump')),

    -- `deny`: refuse, with a status the client can act on and a message that
    -- says why. Both NULL for any other action.
    --
    -- 4xx only, and enforced here rather than in the handler: a rule that
    -- refused with a 5xx would tell every client library in the world to
    -- retry a request that is never going to be allowed, and would show up in
    -- error-rate charts as the gateway failing rather than as policy working.
    ADD COLUMN deny_status integer
        CHECK (deny_status IS NULL OR (deny_status >= 400 AND deny_status < 500)),
    ADD COLUMN deny_message text,

    -- `jump`: continue evaluation in another frontend model's rule chain, so
    -- a shared block of policy is written once instead of repeated per
    -- frontend model.
    --
    -- ON DELETE SET NULL rather than CASCADE: deleting the frontend model a
    -- rule jumps to should not silently delete the rule -- and a jump with
    -- nowhere to go is treated as no match, so the request falls through to
    -- the next rule rather than failing.
    ADD COLUMN jump_to uuid REFERENCES frontend_models(id) ON DELETE SET NULL,

    -- A label carried onto the usage rows this rule produced, for attributing
    -- spend to the decision that caused it. Not an action: a rule that tags
    -- still has to route, or there would be no usage row to tag.
    ADD COLUMN tag text CHECK (tag IS NULL OR length(tag) <= 64);

-- Which rule's tag applies. On the event rather than resolved at read time
-- because a rule can be edited or deleted afterwards, and what a request was
-- attributed to is a fact about when it ran -- the same reason `cost_micros`
-- is stored rather than derived.
ALTER TABLE usage_events ADD COLUMN tag text;

CREATE INDEX usage_events_by_tag ON usage_events (tag, at DESC) WHERE tag IS NOT NULL;
