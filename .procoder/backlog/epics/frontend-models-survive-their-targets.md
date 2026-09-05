# Frontend models survive their targets

Status: done 2026-09-05
Created: 2026-09-04
Milestone: self-registering-hosts
Issue: #14

## Description

Once provider models are deleted automatically, a frontend model can lose what
it points at while remaining the surface every caller names and every grant is
written against. Keeping it is not a nicety: losing it because a Spark rebooted
would take the permissions with it.

Targets therefore bind by name rather than by foreign key. A key gets cascaded
away and the frontend silently forgets what it wanted, with no record of what it
used to be and a manual reattach. Binding to provider and exposed name means the
same swap run again reattaches the frontend by itself, which given how often
models are swapped is the difference between useful and tedious.

The trap it has to solve is that first match wins and a matching rule commits:
if its targets resolve to nothing routable the request fails rather than falling
through, because falling through would make "first match wins" depend on backend
health the rule author cannot see. Static targets essentially never resolve to
nothing; dynamic ones do by design. So a rule with only dynamic targets becomes
a black hole the moment a Spark reboots, and will not fall through to the cloud
rule directly beneath it.

The answer is mixed targets within one rule rather than a change to commit
semantics — a leased-out model is simply not routable and the weighted split
renormalises across the survivors, the same mechanism that already handles a
health-probe rotation.
