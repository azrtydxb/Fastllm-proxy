//! Virtual models: a client-facing name backed by an ordered list of rules.
//!
//! Everything here is pre-resolved into the [`Snapshot`](crate::snapshot::Snapshot)
//! by the control plane, exactly like RBAC grants are (`control::build::flatten_grants`).
//! The request path evaluates a handful of rules in memory — no I/O, no
//! database, no graph walk — which is what keeps this off the latency budget
//! the rest of the proxy protects.
//!
//! ## Weighted split versus prefix affinity
//!
//! A hashed request prefix wants to stick to one backend node (cache
//! locality); a percentage split wants to spread requests across models
//! (canary, A/B). These two goals conflict if left to fight over the same
//! decision, so they are given different jobs: the split decides *which
//! model* a request goes to, and the existing `Router` (`src/router.rs`)
//! decides *which replica* within that model's pool, using its own
//! prefix-affinity policy exactly as it does today.
//!
//! The split itself is **deterministic**, not randomised: [`choose_weighted`]
//! hashes the same request-prefix bytes the backend router already hashes,
//! and picks a target by where that hash falls in the cumulative weight
//! range. The same conversation (same prefix) therefore lands on the same
//! side of a canary on every turn — a coin flip per request would put every
//! multi-turn conversation on a random mix of two model versions and defeat
//! the entire reason a routing rule exists.
//!
//! ## No recursion
//!
//! A virtual model's targets are concrete model names only — `rule_targets`
//! and `virtual_model_defaults` reference `models.id`, not `virtual_models.id`,
//! so a virtual model targeting another virtual model cannot even be
//! expressed in the schema. That sidesteps cycle detection and an
//! evaluation-depth limit entirely, for a feature (virtual models chaining to
//! virtual models) nobody asked for.

use crate::registry::Registry;
use crate::snapshot::{Principal, PrincipalId};
use chrono::{DateTime, Datelike, Timelike, Utc};
use hyper::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// Bytes-per-token used to turn a raw request body size into an estimated
/// prompt token count, per the design doc: real tokenisation on the request
/// path is out of the question (it would mean linking a tokenizer and
/// running it per request, the exact kind of cost this proxy exists to
/// avoid), so this is a rough English-text average, not a real count.
/// **Every value derived from this is an estimate** — callers must not treat
/// it as exact, and nothing here claims otherwise.
pub const ESTIMATED_BYTES_PER_TOKEN: f64 = 3.5;

/// Turn a request body length into an estimated prompt token count.
///
/// Deliberately crude: this counts every byte of the JSON body (field names,
/// punctuation, `"role":"user"` boilerplate included), not just message
/// content, and non-English text or heavy Unicode use will estimate worse
/// than average English prose does. It exists to bucket requests as
/// "roughly small" or "roughly large" for a routing rule, not to bill
/// anyone — real usage accounting (P3) reads the provider's own `usage`
/// object instead.
#[inline]
pub fn estimate_prompt_tokens(body_len: usize) -> u64 {
    (body_len as f64 / ESTIMATED_BYTES_PER_TOKEN).round() as u64
}

/// Which model a request may be sent to, and how much of the split it gets.
///
/// `weight` is a relative share, not a percentage that has to sum to 100 —
/// two targets weighted 1 and 1 split evenly, 1 and 3 split 25/75. This
/// mirrors how `rule_targets`/`virtual_model_defaults` store it, and it means
/// adding a third target never requires rebalancing the other two's numbers
/// to keep them summing to 100.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeightedTarget {
    pub model: String,
    pub weight: u32,
}

/// Who is calling. Empty on both sides means "anyone" — a caller condition
/// with nothing configured must not silently match nobody.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallerMatch {
    pub principals: HashSet<PrincipalId>,
    pub roles: HashSet<String>,
}

impl CallerMatch {
    fn is_empty(&self) -> bool {
        self.principals.is_empty() && self.roles.is_empty()
    }

    /// `caller` is `None` on an open (unauthenticated) snapshot. A rule that
    /// names specific principals or roles cannot match an unidentified
    /// caller — matching it anyway would mean an operator's attempt to
    /// *restrict* a rule to certain callers silently applied it to everyone
    /// instead, on the one deployment shape where identity is undefined.
    fn matches(&self, caller: Option<&Principal>) -> bool {
        if self.is_empty() {
            return true;
        }
        let Some(caller) = caller else {
            return false;
        };
        self.principals.contains(&caller.id) || !self.roles.is_disjoint(&caller.roles)
    }
}

/// Request headers a rule requires.
///
/// Names are lower-cased at snapshot build so the per-request comparison is a
/// plain byte equality rather than a case-insensitive walk — HTTP header names
/// are case-insensitive, and doing that work once per rebuild instead of once
/// per header per request is the same trade the rest of this module makes.
///
/// Every entry must match (AND). This is the condition that delegates the
/// decision to whoever actually knows the workload: a batch job can label
/// itself and be routed accordingly, which no amount of inspecting its prompt
/// could reliably infer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderMatch {
    pub required: Vec<(String, String)>,
}

impl HeaderMatch {
    fn matches(&self, headers: &HeaderMap) -> bool {
        self.required.iter().all(|(name, want)| {
            headers
                .get(name.as_str())
                .and_then(|v| v.to_str().ok())
                .is_some_and(|got| got == want)
        })
    }
}

/// How much of the caller's token budget is already spent.
///
/// Lets a rule degrade service instead of denying it: route a principal past
/// 80% of budget onto a cheaper (or free, local) model rather than letting it
/// run to the 402 cliff. Both bounds are inclusive percentages of
/// `tokens_total`.
///
/// A principal with no budget configured has no percentage — such a request
/// fails any rule that sets a bound, the same way an absent `max_tokens` fails
/// a `max_tokens` bound. Treating "unlimited" as 0% would route the callers
/// with no budget at all through the conserve-budget branch, which is exactly
/// backwards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetMatch {
    pub min_used_percent: Option<u8>,
    pub max_used_percent: Option<u8>,
}

impl BudgetMatch {
    fn is_empty(&self) -> bool {
        self.min_used_percent.is_none() && self.max_used_percent.is_none()
    }

    fn matches(&self, caller: Option<&Principal>) -> bool {
        if self.is_empty() {
            return true;
        }
        let Some(budget) = caller.and_then(|c| c.budget.as_ref()) else {
            return false;
        };
        // Whichever cap is closest to being hit. A budget capping both tokens
        // and spend is "80% used" when *either* is, because that is the one
        // about to refuse requests — taking only tokens would let a rule meant
        // to degrade before the cliff miss a principal running out of money.
        let pct = |used: u64, total: Option<u64>| {
            total.filter(|t| *t > 0).map(|t| {
                used.saturating_mul(100)
                    .saturating_div(t)
                    .min(u64::from(u8::MAX)) as u8
            })
        };
        let Some(used) = pct(budget.tokens_used, budget.tokens_total)
            .into_iter()
            .chain(pct(budget.cost_used_micros, budget.cost_total_micros))
            .max()
        else {
            // No cap at all: there is no percentage of nothing, so a rule
            // keyed on one cannot match rather than matching everything.
            return false;
        };
        if self.min_used_percent.is_some_and(|min| used < min) {
            return false;
        }
        if self.max_used_percent.is_some_and(|max| used > max) {
            return false;
        }
        true
    }
}

/// How busy this rule's own targets are.
///
/// The one condition here that is **not** a pure function of the request: it
/// reads live in-flight counters, so two identical requests a second apart can
/// legitimately route differently and prefix affinity stops meaning anything
/// for the traffic it diverts. That is the price of burst-to-cloud, and it is
/// worth paying knowingly rather than by accident — which is why the field is
/// named for the mechanism (`max_inflight_per_backend`) rather than for the
/// intent ("spill").
///
/// Expressed as a ceiling on the rule's *own* targets so that spilling is
/// written as ordinary first-match-wins ordering:
///
/// ```json
/// [{"targets": ["local"],  "match": {"max_inflight_per_backend": 2}},
///  {"targets": ["cloud"],  "match": {}}]
/// ```
///
/// The local rule stops matching once every healthy local backend is at two
/// in flight, and the next rule catches the overflow. No new concept, no
/// second routing mechanism.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadMatch {
    pub max_inflight_per_backend: Option<u32>,
}

impl LoadMatch {
    /// True when at least one healthy backend across `targets` is below the
    /// ceiling. "At least one" rather than "all": a pool with one idle node
    /// and one busy node still has room, and requiring every node to be under
    /// the limit would spill while capacity sat unused.
    fn matches(&self, targets: &[WeightedTarget], registry: &Registry) -> bool {
        let Some(ceiling) = self.max_inflight_per_backend else {
            return true;
        };
        targets.iter().any(|t| {
            registry.pool(&t.model).is_some_and(|pool| {
                pool.iter()
                    .any(|b| b.is_healthy() && (b.inflight() as u64) < u64::from(ceiling))
            })
        })
    }
}

/// A window of wall-clock time, for "overnight everything stays local".
///
/// `utc_offset_minutes` exists because operators think in local time and this
/// binary carries no timezone database — a fixed offset covers the real case
/// (one deployment, one locale) without pulling in `tz` data or pretending to
/// handle a DST transition it cannot see. Say so in the rule rather than
/// making someone convert 22:00 local into UTC in their head and get it wrong
/// twice a year.
///
/// A window whose end is before its start wraps midnight, which is the shape
/// almost every useful window has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimeMatch {
    /// Minutes past midnight, inclusive.
    pub after_minute: Option<u16>,
    /// Minutes past midnight, exclusive.
    pub before_minute: Option<u16>,
    /// ISO weekdays, 1 = Monday .. 7 = Sunday. Empty means every day.
    pub days: Vec<u8>,
    pub utc_offset_minutes: i16,
}

impl TimeMatch {
    fn is_empty(&self) -> bool {
        self.after_minute.is_none() && self.before_minute.is_none() && self.days.is_empty()
    }

    fn matches(&self, now: DateTime<Utc>) -> bool {
        if self.is_empty() {
            return true;
        }
        let local = now + chrono::Duration::minutes(i64::from(self.utc_offset_minutes));
        if !self.days.is_empty() {
            let weekday = local.weekday().number_from_monday() as u8;
            if !self.days.contains(&weekday) {
                return false;
            }
        }
        let minute = (local.hour() * 60 + local.minute()) as u16;
        match (self.after_minute, self.before_minute) {
            (None, None) => true,
            (Some(after), None) => minute >= after,
            (None, Some(before)) => minute < before,
            // end <= start means the window crosses midnight, so the test
            // flips from "between" to "outside".
            (Some(after), Some(before)) if before <= after => minute >= after || minute < before,
            (Some(after), Some(before)) => minute >= after && minute < before,
        }
    }
}

/// What the prompt is about, as decided by `crate::classifier`.
///
/// The one condition whose input is not read directly off the request: it is
/// the output of a classifier that ran before rule evaluation. Matching is
/// still a string comparison — all the work happened once, before this.
///
/// An unclassified request (below the class's confidence floor, or no
/// classifier in this build) fails any rule that names a class, which is what
/// makes "unclassified falls through to the next rule" true rather than
/// aspirational.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassMatch {
    pub class: Option<String>,
}

impl ClassMatch {
    /// Matches the class itself, or any class that refines it.
    ///
    /// Refinement is a *sub*-classification: `debugging` is a kind of `coding`.
    /// A rule saying "coding goes to the local pool" should therefore still
    /// catch a request the refined tier called `debugging`, and an operator who
    /// wants to split them writes a more specific rule *earlier* in the chain —
    /// which is what rule order is for. The alternative was that defining a
    /// refined class silently stopped every existing rule on its parent from
    /// firing.
    fn matches(&self, classified_as: Option<&str>, refines: &[String]) -> bool {
        match &self.class {
            None => true,
            Some(want) => classified_as == Some(want.as_str()) || refines.iter().any(|r| r == want),
        }
    }
}

/// Approximate request shape: estimated prompt size and requested
/// `max_tokens`, both already available before routing — `max_tokens` is read
/// from the same JSON body already parsed once for `model`
/// (`proxy::BodyPeek`), and the prompt estimate is [`estimate_prompt_tokens`]
/// over the raw body length already in hand. Each bound is inclusive and
/// `None` means unconstrained; an unset `max_tokens` in the request body
/// fails any bound that requires one, the same way a caller condition fails
/// to match an unidentified caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShapeMatch {
    pub min_prompt_tokens: Option<u64>,
    pub max_prompt_tokens: Option<u64>,
    pub min_max_tokens: Option<u64>,
    pub max_max_tokens: Option<u64>,
    /// Whether the client asked for a stream. A useful proxy for "is a human
    /// waiting": a non-streaming request is almost always a batch job, and
    /// batch work is exactly what should be pushed to the slower or cheaper
    /// target when the fast one is contended.
    pub stream: Option<bool>,
}

impl ShapeMatch {
    fn matches(
        &self,
        estimated_prompt_tokens: u64,
        requested_max_tokens: Option<u64>,
        streaming: bool,
    ) -> bool {
        if self.stream.is_some_and(|want| want != streaming) {
            return false;
        }
        if self
            .min_prompt_tokens
            .is_some_and(|min| estimated_prompt_tokens < min)
        {
            return false;
        }
        if self
            .max_prompt_tokens
            .is_some_and(|max| estimated_prompt_tokens > max)
        {
            return false;
        }
        if self.min_max_tokens.is_some() || self.max_max_tokens.is_some() {
            let Some(requested) = requested_max_tokens else {
                return false;
            };
            if self.min_max_tokens.is_some_and(|min| requested < min) {
                return false;
            }
            if self.max_max_tokens.is_some_and(|max| requested > max) {
                return false;
            }
        }
        true
    }
}

/// JSON shape of a rule's match condition — what `routing_rules.match_json`
/// stores and what `POST .../rules` accepts, combining [`CallerMatch`] and
/// [`ShapeMatch`] into the one object an operator writes.
///
/// Kept distinct from `CallerMatch`/`ShapeMatch` themselves (`HashSet`
/// fields, no natural JSON array order) rather than deriving
/// `Serialize`/`Deserialize` on those directly — this is the wire/storage
/// representation, they are the request-path representation, and collapsing
/// the two would mean either the hot-path types carry `serde` derives they
/// never otherwise need, or JSON round-trips through an order-sensitive
/// `Vec` that then has to be deduplicated on every snapshot build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MatchConditionJson {
    #[serde(default)]
    pub principals: Vec<PrincipalId>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub min_prompt_tokens: Option<u64>,
    #[serde(default)]
    pub max_prompt_tokens: Option<u64>,
    #[serde(default)]
    pub min_max_tokens: Option<u64>,
    #[serde(default)]
    pub max_max_tokens: Option<u64>,
    #[serde(default)]
    pub stream: Option<bool>,
    /// Header name to required value. Names are lower-cased here, once, so
    /// the request path compares bytes.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub min_budget_used_percent: Option<u8>,
    #[serde(default)]
    pub max_budget_used_percent: Option<u8>,
    #[serde(default)]
    pub max_inflight_per_backend: Option<u32>,
    /// `"HH:MM"`, in the timezone `utc_offset_minutes` describes.
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub before: Option<String>,
    /// ISO weekdays, 1 = Monday .. 7 = Sunday. Empty means every day.
    #[serde(default)]
    pub days: Vec<u8>,
    #[serde(default)]
    pub utc_offset_minutes: i16,
    /// Name of a prompt class this rule requires. See `crate::classifier`.
    #[serde(default)]
    pub class: Option<String>,
}

/// `"HH:MM"` to minutes past midnight.
///
/// A malformed value yields `None`, which makes that half of the window
/// unbounded rather than failing the whole rule — but `MatchConditionJson`'s
/// admin-side validation rejects it before it can be stored, so this is the
/// belt to that braces.
fn parse_hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    let h: u16 = h.trim().parse().ok()?;
    let m: u16 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// Whether every time/percentage field in this condition is well formed.
///
/// Called by the admin API so a typo is a 400 at write time rather than a rule
/// that silently never matches — the failure mode this repo keeps finding by
/// review instead of by test.
pub fn validate_match_json(c: &MatchConditionJson) -> Result<(), String> {
    for (field, value) in [("after", &c.after), ("before", &c.before)] {
        if let Some(v) = value {
            if parse_hhmm(v).is_none() {
                return Err(format!("{field} must be \"HH:MM\" (24-hour), got {v:?}"));
            }
        }
    }
    for d in &c.days {
        if !(1..=7).contains(d) {
            return Err(format!(
                "days must be ISO weekdays 1 (Monday) to 7 (Sunday), got {d}"
            ));
        }
    }
    for (field, value) in [
        ("min_budget_used_percent", c.min_budget_used_percent),
        ("max_budget_used_percent", c.max_budget_used_percent),
    ] {
        if value.is_some_and(|v| v > 100) {
            return Err(format!("{field} must be between 0 and 100"));
        }
    }
    if c.class.as_deref().is_some_and(str::is_empty) {
        return Err("class must not be empty".to_string());
    }
    if !(-1440..=1440).contains(&c.utc_offset_minutes) {
        return Err("utc_offset_minutes must be between -1440 and 1440".to_string());
    }
    Ok(())
}

impl MatchConditionJson {
    /// Flatten the stored JSON into the shapes the request path matches on.
    ///
    /// All the parsing — header-name casing, `"HH:MM"` arithmetic — happens
    /// here, at snapshot build, so nothing in `RoutingRule::matches` does more
    /// than compare integers and bytes.
    pub fn into_conditions(self) -> RuleConditions {
        RuleConditions {
            caller: CallerMatch {
                principals: self.principals.into_iter().collect(),
                roles: self.roles.into_iter().collect(),
            },
            shape: ShapeMatch {
                min_prompt_tokens: self.min_prompt_tokens,
                max_prompt_tokens: self.max_prompt_tokens,
                min_max_tokens: self.min_max_tokens,
                max_max_tokens: self.max_max_tokens,
                stream: self.stream,
            },
            headers: HeaderMatch {
                required: self
                    .headers
                    .into_iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), v))
                    .collect(),
            },
            budget: BudgetMatch {
                min_used_percent: self.min_budget_used_percent,
                max_used_percent: self.max_budget_used_percent,
            },
            load: LoadMatch {
                max_inflight_per_backend: self.max_inflight_per_backend,
            },
            time: TimeMatch {
                after_minute: self.after.as_deref().and_then(parse_hhmm),
                before_minute: self.before.as_deref().and_then(parse_hhmm),
                days: self.days,
                utc_offset_minutes: self.utc_offset_minutes,
            },
            class: ClassMatch { class: self.class },
        }
    }
}

/// Every condition of one rule, already parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleConditions {
    pub caller: CallerMatch,
    pub shape: ShapeMatch,
    pub headers: HeaderMatch,
    pub budget: BudgetMatch,
    pub load: LoadMatch,
    pub time: TimeMatch,
    pub class: ClassMatch,
}

/// One rule in a virtual model's ordered chain: match conditions AND'd
/// together, and the targets to route to when they all hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingRule {
    pub conditions: RuleConditions,
    pub targets: Vec<WeightedTarget>,
}

/// Everything a rule is matched against, gathered once per request.
///
/// A struct rather than eight positional arguments: this list has grown from
/// two conditions to six, and a call site that silently transposed two `u64`s
/// would route wrongly without failing to compile.
pub struct RequestFacts<'a> {
    pub caller: Option<&'a Principal>,
    pub prompt_tokens: u64,
    pub max_tokens: Option<u64>,
    pub streaming: bool,
    pub headers: &'a HeaderMap,
    pub now: DateTime<Utc>,
    /// What the classifier decided, or `None` when the request was not
    /// classified — no classes configured, no classifier in this build, or the
    /// best class did not clear its confidence floor.
    pub class: Option<&'a str>,
    /// The classes `class` refines, so a rule on the general class still
    /// matches a refined answer.
    pub class_refines: &'a [String],
}

impl RoutingRule {
    /// Conditions are AND'd, cheapest first: the two that read nothing but
    /// integers already in hand short-circuit before the ones that walk
    /// headers or pool counters.
    /// Whether this rule's conditions hold. `pub` so the control plane's
    /// routing dry-run can report *which* rule decided, not only what it
    /// resolved to — "why did this route there" is the question a rule author
    /// has, and the answer is a rule index.
    pub fn matches(&self, facts: &RequestFacts<'_>, registry: &Registry) -> bool {
        let c = &self.conditions;
        c.shape
            .matches(facts.prompt_tokens, facts.max_tokens, facts.streaming)
            && c.class.matches(facts.class, facts.class_refines)
            && c.caller.matches(facts.caller)
            && c.budget.matches(facts.caller)
            && c.time.matches(facts.now)
            && c.headers.matches(facts.headers)
            && c.load.matches(&self.targets, registry)
    }
}

/// A client-facing name with an ordered list of rules and a fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VirtualModelDef {
    pub name: String,
    /// Evaluated in order; the first whose conditions match wins.
    pub rules: Vec<RoutingRule>,
    /// Used when no rule matches. Never consulted otherwise, even if a
    /// matching rule's own targets turn out to be unroutable (see
    /// [`VirtualModelDef::resolve`]'s doc comment).
    pub default_targets: Vec<WeightedTarget>,
}

impl VirtualModelDef {
    /// Resolve to one concrete model name.
    ///
    /// `prefix_hash` must be the same hash the request's backend routing
    /// uses (`Router::prefix_key`) — reusing it, rather than computing a
    /// second one, is what makes the *target* choice and the *replica*
    /// choice both deterministic on the same request bytes without adding a
    /// second pass over the body.
    ///
    /// A matching rule commits to its own targets: if they resolve to
    /// nothing routable, this returns `None` rather than falling through to
    /// the next rule or to the defaults. Falling through on failure would
    /// make "first match wins" a lie — a rule that matched would sometimes
    /// not be the rule that decided anything, silently, based on backend
    /// health an operator reading the rule list has no way to see.
    pub fn resolve(
        &self,
        facts: &RequestFacts<'_>,
        prefix_hash: u64,
        registry: &Registry,
    ) -> Option<String> {
        self.resolve_candidates(facts, prefix_hash, registry)
            .into_iter()
            .next()
    }

    /// The matching rule's targets, ordered best-first.
    ///
    /// The head of this list is exactly what `resolve` used to return alone:
    /// the weighted pick, or the first healthy alternative if that pool has no
    /// healthy backend. What is new is the *tail* — the remaining targets, so
    /// `proxy_request` can fail over to another model when the first one turns
    /// out to be unusable at request time rather than at resolution time.
    ///
    /// That distinction is the whole point. Health is a background signal,
    /// minutes stale at worst and blind to the case that actually bites here:
    /// a provider answering 429 right now. A pool can be perfectly healthy and
    /// still refuse this request, and before this list existed there was
    /// nowhere for the request to go.
    ///
    /// Still first-match-wins: only the matching rule's targets appear. A rule
    /// that matched never falls through to the next rule's targets, because
    /// then a rule that matched would not be the rule that decided anything.
    pub fn resolve_candidates(
        &self,
        facts: &RequestFacts<'_>,
        prefix_hash: u64,
        registry: &Registry,
    ) -> Vec<String> {
        for rule in &self.rules {
            if rule.matches(facts, registry) {
                return order_candidates(&rule.targets, prefix_hash, registry);
            }
        }
        order_candidates(&self.default_targets, prefix_hash, registry)
    }
}

/// Deterministically choose one target by where `prefix_hash` falls in the
/// cumulative weight range.
///
/// Not RNG: the design's stated reason is cache locality across turns of the
/// same conversation (see the module doc comment). A weight of zero on every
/// target (or an empty list) has nothing meaningful to divide, so the first
/// target is returned as a defined fallback rather than this function
/// panicking on a divide-by-zero.
pub fn choose_weighted(targets: &[WeightedTarget], prefix_hash: u64) -> Option<&WeightedTarget> {
    let total: u64 = targets.iter().map(|t| u64::from(t.weight)).sum();
    if total == 0 {
        return targets.first();
    }
    let mut remainder = prefix_hash % total;
    for t in targets {
        let w = u64::from(t.weight);
        if remainder < w {
            return Some(t);
        }
        remainder -= w;
    }
    // Unreachable when `total` is the true sum of every weight, but a tail
    // return is cheaper to read than proving the loop above exhaustive.
    targets.last()
}

/// Pick a target, falling through an unhealthy or saturated one to the next
/// in the chain.
///
/// "Saturated" is answered by [`Registry::pool_has_healthy`] — the pool for
/// that model exists and has at least one backend not currently marked
/// unhealthy. This deliberately does not look at in-flight counts: that
/// finer-grained load balancing already happens one layer down, inside the
/// chosen model's own pool (`Router::pick`), and duplicating it here would
/// just be a second, cruder copy of the same decision.
///
/// When nothing in the chain is healthy, the weighted pick is still
/// returned rather than `None` — the same "last resort beats a synthetic
/// 503" rule `Router::pick` follows for backends within one pool. The
/// `proxy_request` retry loop is what eventually turns a truly dead target
/// into a `502`, exactly as it does today for an ordinary (non-virtual)
/// model with every backend down.
fn order_candidates(
    targets: &[WeightedTarget],
    prefix_hash: u64,
    registry: &Registry,
) -> Vec<String> {
    let Some(chosen) = choose_weighted(targets, prefix_hash) else {
        return Vec::new();
    };
    // Weighted pick first, then the rest in declaration order — declaration
    // order is the operator's stated preference, and honouring it is what
    // makes a target list readable as a fallback chain.
    let mut ordered: Vec<String> = std::iter::once(chosen.model.clone())
        .chain(
            targets
                .iter()
                .map(|t| t.model.clone())
                .filter(|m| *m != chosen.model),
        )
        .collect();
    ordered.dedup();
    // Stable, so a healthy pool is preferred without disturbing the relative
    // order of equals. Unhealthy targets are kept rather than dropped: when
    // nothing is healthy the request still goes somewhere and the upstream's
    // own error reaches the client, which beats a synthetic 503 — the same
    // last-resort rule `Router::pick` follows inside a single pool.
    ordered.sort_by_key(|m| !registry.pool_has_healthy(m));
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileConfig;
    use crate::registry::Interner;
    use crate::snapshot::Principal;
    use std::collections::HashSet as Set;
    use std::sync::Arc;

    fn registry_with(models: &[&str]) -> Registry {
        let entries = models
            .iter()
            .map(|m| {
                format!(
                    "  - model_name: {m}\n    litellm_params: {{ api_base: \"http://10.0.0.1:8000/v1\", model: {m} }}\n"
                )
            })
            .collect::<String>();
        let yaml = format!("model_list:\n{entries}");
        let cfg: FileConfig = serde_yaml::from_str(&yaml).unwrap();
        Registry::build(&cfg, &Interner::default(), None).unwrap()
    }

    /// The request side of a match, with everything a test does not care
    /// about set to "matches anything". A helper rather than a literal at each
    /// call site because `RequestFacts` borrows its headers, and a temporary
    /// `HeaderMap` inline would not outlive the call.
    fn facts_with<'a>(
        caller: Option<&'a Principal>,
        prompt_tokens: u64,
        max_tokens: Option<u64>,
        headers: &'a HeaderMap,
    ) -> RequestFacts<'a> {
        RequestFacts {
            caller,
            prompt_tokens,
            max_tokens,
            streaming: false,
            headers,
            now: fixed_now(),
            class: None,
            class_refines: &[],
        }
    }

    /// Wednesday 2026-08-05, 12:00 UTC — a fixed instant so time-window tests
    /// assert on arithmetic rather than on when the suite happens to run.
    fn fixed_now() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 5, 12, 0, 0).unwrap()
    }

    fn target(model: &str, weight: u32) -> WeightedTarget {
        WeightedTarget {
            model: model.to_string(),
            weight,
        }
    }

    fn principal(id: PrincipalId, roles: &[&str]) -> Principal {
        Principal {
            id,
            name: format!("p{id}"),
            allowed_models: Set::new(),
            allow_all: true,
            roles: roles.iter().map(|r| r.to_string()).collect(),
            limits: None,
            budget: None,
        }
    }

    // --- rule ordering -----------------------------------------------

    #[test]
    fn the_first_matching_rule_wins() {
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![
                RoutingRule {
                    conditions: RuleConditions::default(),
                    targets: vec![target("first", 1)],
                },
                RoutingRule {
                    conditions: RuleConditions::default(),
                    targets: vec![target("second", 1)],
                },
            ],
            default_targets: vec![target("default", 1)],
        };
        let reg = registry_with(&["first", "second", "default"]);
        assert_eq!(
            vm.resolve(&facts_with(None, 0, None, &HeaderMap::new()), 0, &reg)
                .as_deref(),
            Some("first"),
            "both rules match everything; the first one in order must decide"
        );
    }

    #[test]
    fn a_later_rule_is_used_when_an_earlier_one_does_not_match() {
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![
                RoutingRule {
                    conditions: RuleConditions {
                        caller: CallerMatch {
                            principals: [999].into_iter().collect(),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    targets: vec![target("only-for-999", 1)],
                },
                RoutingRule {
                    conditions: RuleConditions::default(),
                    targets: vec![target("catch-all", 1)],
                },
            ],
            default_targets: vec![],
        };
        let reg = registry_with(&["only-for-999", "catch-all"]);
        let caller = principal(1, &[]);
        assert_eq!(
            vm.resolve(
                &facts_with(Some(&caller), 0, None, &HeaderMap::new()),
                0,
                &reg
            )
            .as_deref(),
            Some("catch-all")
        );
    }

    // --- match conditions in isolation --------------------------------

    #[test]
    fn caller_match_by_principal_id() {
        let m = CallerMatch {
            principals: [1].into_iter().collect(),
            roles: Set::new(),
        };
        assert!(m.matches(Some(&principal(1, &[]))));
        assert!(!m.matches(Some(&principal(2, &[]))));
        assert!(
            !m.matches(None),
            "an unidentified caller cannot match a named one"
        );
    }

    #[test]
    fn caller_match_by_role() {
        let m = CallerMatch {
            principals: Set::new(),
            roles: ["beta".to_string()].into_iter().collect(),
        };
        assert!(m.matches(Some(&principal(1, &["beta", "other"]))));
        assert!(!m.matches(Some(&principal(1, &["other"]))));
    }

    #[test]
    fn an_empty_caller_match_matches_anyone_including_unauthenticated() {
        let m = CallerMatch::default();
        assert!(m.matches(Some(&principal(1, &[]))));
        assert!(m.matches(None));
    }

    #[test]
    fn shape_match_on_estimated_prompt_size() {
        let m = ShapeMatch {
            max_prompt_tokens: Some(100),
            ..Default::default()
        };
        assert!(m.matches(50, None, false));
        assert!(!m.matches(101, None, false));
    }

    #[test]
    fn shape_match_on_requested_max_tokens() {
        let m = ShapeMatch {
            max_max_tokens: Some(256),
            ..Default::default()
        };
        assert!(m.matches(0, Some(100), false));
        assert!(!m.matches(0, Some(257), false));
        assert!(
            !m.matches(0, None, false),
            "a bound on max_tokens cannot be satisfied by a request that omitted it"
        );
    }

    #[test]
    fn combined_caller_and_shape_conditions_both_must_hold() {
        let reg = registry_with(&["m"]);
        let rule = RoutingRule {
            conditions: RuleConditions {
                caller: CallerMatch {
                    roles: ["canary".to_string()].into_iter().collect(),
                    ..Default::default()
                },
                shape: ShapeMatch {
                    max_prompt_tokens: Some(1000),
                    ..Default::default()
                },
                ..Default::default()
            },
            targets: vec![target("m", 1)],
        };
        let canary = principal(1, &["canary"]);
        let other = principal(2, &["other"]);
        assert!(
            rule.matches(
                &facts_with(Some(&canary), 500, None, &HeaderMap::new()),
                &reg
            ),
            "role ok, shape ok"
        );
        assert!(
            !rule.matches(
                &facts_with(Some(&other), 500, None, &HeaderMap::new()),
                &reg
            ),
            "shape ok but wrong role"
        );
        assert!(
            !rule.matches(
                &facts_with(Some(&canary), 5000, None, &HeaderMap::new()),
                &reg
            ),
            "right role but shape too large"
        );
    }

    // --- weighted split: determinism and distribution -------------------

    #[test]
    fn the_same_prefix_always_resolves_to_the_same_target() {
        let targets = vec![target("a", 30), target("b", 70)];
        let reg = registry_with(&["a", "b"]);
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![],
            default_targets: targets,
        };
        let prefix = 0xdead_beef_1234_5678u64;
        let first = vm.resolve(&facts_with(None, 0, None, &HeaderMap::new()), prefix, &reg);
        for _ in 0..200 {
            assert_eq!(
                vm.resolve(&facts_with(None, 0, None, &HeaderMap::new()), prefix, &reg),
                first,
                "the same request prefix must land on the same target every time"
            );
        }
    }

    #[test]
    fn distinct_prefixes_distribute_close_to_the_configured_weights() {
        let targets = vec![target("a", 1), target("b", 3)];
        let reg = registry_with(&["a", "b"]);
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![],
            default_targets: targets,
        };
        let mut a = 0u32;
        let mut b = 0u32;
        const N: u32 = 4000;
        for i in 0..N {
            // A cheap, well-distributed stand-in for hashing N distinct
            // request bodies: the router's own `fxhash` is exercised
            // directly in `router.rs`'s tests, so this test's job is only
            // to prove `choose_weighted` divides *whatever* hash space it is
            // given proportionally to weight, not to re-test the hash.
            let prefix =
                crate::router::Router::new(crate::router::Policy::CacheAffinity, 64, 64, 0, 0.0)
                    .prefix_key(format!("distinct-body-{i}").as_bytes());
            match vm
                .resolve(&facts_with(None, 0, None, &HeaderMap::new()), prefix, &reg)
                .as_deref()
            {
                Some("a") => a += 1,
                Some("b") => b += 1,
                other => panic!("unexpected target {other:?}"),
            }
        }
        let ratio = f64::from(b) / f64::from(a.max(1));
        // Configured 1:3 => ~3.0; wide tolerance because this is a hash
        // distribution over a few thousand samples, not an exact split.
        assert!(
            (2.0..4.5).contains(&ratio),
            "expected roughly a 1:3 split, got {a}:{b} (ratio {ratio:.2})"
        );
    }

    // --- failover ---------------------------------------------------

    #[test]
    fn an_unhealthy_primary_falls_through_to_the_next_target() {
        let reg = registry_with(&["primary", "secondary"]);
        reg.pool("primary").unwrap()[0].mark_probe_failed(1);
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![],
            // weight 100 on primary so `choose_weighted` always picks it
            // first, isolating the failover behaviour from the split.
            default_targets: vec![target("primary", 100), target("secondary", 1)],
        };
        assert_eq!(
            vm.resolve(&facts_with(None, 0, None, &HeaderMap::new()), 0, &reg)
                .as_deref(),
            Some("secondary"),
            "an unhealthy primary must fall through to the next target in the chain"
        );
    }

    #[test]
    fn a_healthy_primary_is_not_bypassed() {
        let reg = registry_with(&["primary", "secondary"]);
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![],
            default_targets: vec![target("primary", 100), target("secondary", 1)],
        };
        assert_eq!(
            vm.resolve(&facts_with(None, 0, None, &HeaderMap::new()), 0, &reg)
                .as_deref(),
            Some("primary")
        );
    }

    #[test]
    fn every_target_unhealthy_still_returns_the_weighted_choice_as_a_last_resort() {
        let reg = registry_with(&["primary", "secondary"]);
        reg.pool("primary").unwrap()[0].mark_probe_failed(1);
        reg.pool("secondary").unwrap()[0].mark_probe_failed(1);
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![],
            default_targets: vec![target("primary", 100), target("secondary", 1)],
        };
        assert_eq!(
            vm.resolve(&facts_with(None, 0, None, &HeaderMap::new()), 0, &reg)
                .as_deref(),
            Some("primary"),
            "nothing healthy: fall back to the weighted pick rather than None, \
             same as Router::pick's last-resort rule"
        );
    }

    // --- header conditions ---------------------------------------------

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                hyper::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                hyper::header::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn rule_with(conditions: RuleConditions, model: &str) -> RoutingRule {
        RoutingRule {
            conditions,
            targets: vec![target(model, 1)],
        }
    }

    #[test]
    fn a_header_condition_matches_only_when_the_header_is_present_and_equal() {
        let reg = registry_with(&["m"]);
        let rule = rule_with(
            RuleConditions {
                headers: HeaderMatch {
                    required: vec![("x-fastllm-tier".into(), "batch".into())],
                },
                ..Default::default()
            },
            "m",
        );
        let batch = headers_with(&[("x-fastllm-tier", "batch")]);
        let interactive = headers_with(&[("x-fastllm-tier", "interactive")]);
        let absent = HeaderMap::new();
        assert!(rule.matches(&facts_with(None, 0, None, &batch), &reg));
        assert!(!rule.matches(&facts_with(None, 0, None, &interactive), &reg));
        assert!(
            !rule.matches(&facts_with(None, 0, None, &absent), &reg),
            "an absent header must not match a rule that requires one"
        );
    }

    /// Header names are case-insensitive in HTTP, and the lower-casing happens
    /// once at snapshot build rather than per request.
    #[test]
    fn a_header_condition_ignores_the_case_of_the_name() {
        let reg = registry_with(&["m"]);
        let conditions = MatchConditionJson {
            headers: [("X-FastLLM-Tier".to_string(), "batch".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        }
        .into_conditions();
        let rule = rule_with(conditions, "m");
        let sent = headers_with(&[("x-fastllm-tier", "batch")]);
        assert!(rule.matches(&facts_with(None, 0, None, &sent), &reg));
    }

    #[test]
    fn every_required_header_must_match_not_merely_one() {
        let reg = registry_with(&["m"]);
        let rule = rule_with(
            RuleConditions {
                headers: HeaderMatch {
                    required: vec![("a".into(), "1".into()), ("b".into(), "2".into())],
                },
                ..Default::default()
            },
            "m",
        );
        assert!(rule.matches(
            &facts_with(None, 0, None, &headers_with(&[("a", "1"), ("b", "2")])),
            &reg
        ));
        assert!(!rule.matches(
            &facts_with(None, 0, None, &headers_with(&[("a", "1")])),
            &reg
        ));
    }

    // --- streaming condition --------------------------------------------

    #[test]
    fn a_stream_condition_separates_interactive_from_batch_requests() {
        let reg = registry_with(&["m"]);
        let batch_only = rule_with(
            RuleConditions {
                shape: ShapeMatch {
                    stream: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
            "m",
        );
        let headers = HeaderMap::new();
        let mut streaming = facts_with(None, 0, None, &headers);
        streaming.streaming = true;
        let batch = facts_with(None, 0, None, &headers);
        assert!(batch_only.matches(&batch, &reg));
        assert!(!batch_only.matches(&streaming, &reg));
    }

    // --- budget conditions ----------------------------------------------

    fn principal_with_budget(used: u64, total: u64) -> Principal {
        let mut p = principal(1, &[]);
        p.budget = Some(crate::snapshot::Budget {
            tokens_total: Some(total),
            cost_total_micros: None,
            cost_used_micros: 0,
            tokens_used: used,
        });
        p
    }

    #[test]
    fn a_budget_condition_matches_on_the_percentage_consumed() {
        let reg = registry_with(&["m"]);
        let nearly_spent = rule_with(
            RuleConditions {
                budget: BudgetMatch {
                    min_used_percent: Some(80),
                    ..Default::default()
                },
                ..Default::default()
            },
            "m",
        );
        let headers = HeaderMap::new();
        let at_90 = principal_with_budget(900, 1000);
        let at_50 = principal_with_budget(500, 1000);
        assert!(nearly_spent.matches(&facts_with(Some(&at_90), 0, None, &headers), &reg));
        assert!(!nearly_spent.matches(&facts_with(Some(&at_50), 0, None, &headers), &reg));
    }

    /// The direction that matters: a principal with *no* budget is unlimited,
    /// and must not be treated as 0% consumed and swept into the
    /// conserve-budget branch — which is what a naive `unwrap_or(0)` would do.
    #[test]
    fn a_principal_without_a_budget_fails_a_budget_condition_rather_than_reading_as_zero() {
        let reg = registry_with(&["m"]);
        let headers = HeaderMap::new();
        let unlimited = principal(1, &[]);
        let conserve = rule_with(
            RuleConditions {
                budget: BudgetMatch {
                    min_used_percent: Some(80),
                    ..Default::default()
                },
                ..Default::default()
            },
            "m",
        );
        let plenty_left = rule_with(
            RuleConditions {
                budget: BudgetMatch {
                    max_used_percent: Some(50),
                    ..Default::default()
                },
                ..Default::default()
            },
            "m",
        );
        assert!(!conserve.matches(&facts_with(Some(&unlimited), 0, None, &headers), &reg));
        assert!(
            !plenty_left.matches(&facts_with(Some(&unlimited), 0, None, &headers), &reg),
            "unlimited is not 0% used; both bounds must decline to match"
        );
    }

    // --- load conditions -------------------------------------------------

    /// A pool with one idle node still has room, even if another node is busy —
    /// requiring *every* backend to be under the ceiling would spill traffic
    /// to the cloud while local capacity sat unused.
    #[test]
    fn a_load_condition_matches_while_any_healthy_backend_is_below_the_ceiling() {
        let reg = registry_with(&["busy"]);
        let rule = rule_with(
            RuleConditions {
                load: LoadMatch {
                    max_inflight_per_backend: Some(2),
                },
                ..Default::default()
            },
            "busy",
        );
        let headers = HeaderMap::new();
        assert!(
            rule.matches(&facts_with(None, 0, None, &headers), &reg),
            "an idle pool has room"
        );

        let pool = reg.pool("busy").unwrap();
        let _g1 = crate::registry::InflightGuard::acquire(Arc::clone(&pool[0]));
        let _g2 = crate::registry::InflightGuard::acquire(Arc::clone(&pool[0]));
        assert!(
            !rule.matches(&facts_with(None, 0, None, &headers), &reg),
            "at the ceiling the rule stops matching, so the next rule catches the overflow"
        );
        drop(_g2);
        assert!(
            rule.matches(&facts_with(None, 0, None, &headers), &reg),
            "and it matches again as soon as capacity frees up"
        );
    }

    /// The composition this condition exists for: local while it has room,
    /// cloud when it does not, written as ordinary rule ordering.
    #[test]
    fn a_saturated_local_rule_spills_to_the_next_rule() {
        let reg = registry_with(&["local", "cloud"]);
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![
                rule_with(
                    RuleConditions {
                        load: LoadMatch {
                            max_inflight_per_backend: Some(1),
                        },
                        ..Default::default()
                    },
                    "local",
                ),
                rule_with(RuleConditions::default(), "cloud"),
            ],
            default_targets: vec![],
        };
        let headers = HeaderMap::new();
        assert_eq!(
            vm.resolve(&facts_with(None, 0, None, &headers), 0, &reg)
                .as_deref(),
            Some("local")
        );
        let pool = reg.pool("local").unwrap();
        let _busy = crate::registry::InflightGuard::acquire(Arc::clone(&pool[0]));
        assert_eq!(
            vm.resolve(&facts_with(None, 0, None, &headers), 0, &reg)
                .as_deref(),
            Some("cloud"),
            "the local rule stopped matching, so first-match-wins moved on"
        );
    }

    // --- time conditions -------------------------------------------------

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 5, hour, minute, 0).unwrap()
    }

    fn time_matches(t: &TimeMatch, now: DateTime<Utc>) -> bool {
        let headers = HeaderMap::new();
        let reg = registry_with(&["m"]);
        let rule = rule_with(
            RuleConditions {
                time: t.clone(),
                ..Default::default()
            },
            "m",
        );
        let mut facts = facts_with(None, 0, None, &headers);
        facts.now = now;
        rule.matches(&facts, &reg)
    }

    #[test]
    fn a_daytime_window_matches_inside_and_not_outside() {
        let window = TimeMatch {
            after_minute: Some(9 * 60),
            before_minute: Some(17 * 60),
            ..Default::default()
        };
        assert!(time_matches(&window, at(12, 0)));
        assert!(!time_matches(&window, at(8, 59)));
        assert!(
            !time_matches(&window, at(17, 0)),
            "the end of the window is exclusive"
        );
        assert!(time_matches(&window, at(9, 0)), "the start is inclusive");
    }

    /// The shape almost every useful window has: 22:00 to 06:00 crosses
    /// midnight, so "between start and end" is the wrong test.
    #[test]
    fn an_overnight_window_wraps_past_midnight() {
        let overnight = TimeMatch {
            after_minute: Some(22 * 60),
            before_minute: Some(6 * 60),
            ..Default::default()
        };
        assert!(time_matches(&overnight, at(23, 30)));
        assert!(time_matches(&overnight, at(2, 0)));
        assert!(!time_matches(&overnight, at(12, 0)));
    }

    #[test]
    fn a_utc_offset_shifts_the_window_into_the_operators_local_time() {
        // 22:00-06:00 local at UTC+2 is 20:00-04:00 UTC.
        let overnight_local = TimeMatch {
            after_minute: Some(22 * 60),
            before_minute: Some(6 * 60),
            utc_offset_minutes: 120,
            ..Default::default()
        };
        assert!(
            time_matches(&overnight_local, at(20, 30)),
            "20:30 UTC is 22:30 local, inside the window"
        );
        assert!(
            !time_matches(&overnight_local, at(19, 0)),
            "19:00 UTC is 21:00 local, still outside"
        );
    }

    #[test]
    fn a_days_condition_selects_weekdays() {
        // 2026-08-05 is a Wednesday (ISO weekday 3).
        let weekdays = TimeMatch {
            days: vec![1, 2, 3, 4, 5],
            ..Default::default()
        };
        let weekend = TimeMatch {
            days: vec![6, 7],
            ..Default::default()
        };
        assert!(time_matches(&weekdays, at(12, 0)));
        assert!(!time_matches(&weekend, at(12, 0)));
    }

    // --- validation ------------------------------------------------------

    #[test]
    fn malformed_conditions_are_rejected_with_a_message_naming_the_field() {
        let bad_time = MatchConditionJson {
            after: Some("25:00".into()),
            ..Default::default()
        };
        let err = validate_match_json(&bad_time).unwrap_err();
        assert!(err.contains("after"), "{err}");

        let bad_day = MatchConditionJson {
            days: vec![8],
            ..Default::default()
        };
        assert!(validate_match_json(&bad_day).unwrap_err().contains("days"));

        let bad_percent = MatchConditionJson {
            min_budget_used_percent: Some(101),
            ..Default::default()
        };
        assert!(validate_match_json(&bad_percent)
            .unwrap_err()
            .contains("min_budget_used_percent"));

        assert!(validate_match_json(&MatchConditionJson {
            after: Some("22:00".into()),
            before: Some("06:30".into()),
            days: vec![1, 7],
            min_budget_used_percent: Some(0),
            max_budget_used_percent: Some(100),
            ..Default::default()
        })
        .is_ok());
    }

    // --- failover chains --------------------------------------------------

    /// The tail of the candidate list is what makes runtime failover possible:
    /// health is a background signal, and a pool can be perfectly healthy while
    /// still refusing this particular request with a 429.
    #[test]
    fn resolve_candidates_returns_the_whole_chain_not_just_the_winner() {
        let reg = registry_with(&["primary", "secondary"]);
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![],
            default_targets: vec![target("primary", 100), target("secondary", 0)],
        };
        let headers = HeaderMap::new();
        let chain = vm.resolve_candidates(&facts_with(None, 0, None, &headers), 0, &reg);
        assert_eq!(chain, vec!["primary".to_string(), "secondary".to_string()]);
    }

    #[test]
    fn a_candidate_chain_never_repeats_a_model() {
        let reg = registry_with(&["a"]);
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![],
            default_targets: vec![target("a", 1), target("a", 1)],
        };
        let headers = HeaderMap::new();
        let chain = vm.resolve_candidates(&facts_with(None, 0, None, &headers), 0, &reg);
        assert_eq!(chain, vec!["a".to_string()]);
    }

    // --- class conditions ------------------------------------------------

    #[test]
    fn a_class_rule_matches_the_class_itself() {
        let reg = registry_with(&["m"]);
        let rule = rule_with(
            RuleConditions {
                class: ClassMatch {
                    class: Some("coding".into()),
                },
                ..Default::default()
            },
            "m",
        );
        let headers = HeaderMap::new();
        let mut facts = facts_with(None, 0, None, &headers);
        facts.class = Some("coding");
        assert!(rule.matches(&facts, &reg));

        facts.class = Some("chat");
        assert!(!rule.matches(&facts, &reg));

        facts.class = None;
        assert!(
            !rule.matches(&facts, &reg),
            "an unclassified request must fall through, not match"
        );
    }

    /// Refinement is a sub-classification: a rule on `coding` must keep
    /// matching once `debugging` exists, or adding a refined class silently
    /// breaks every rule on its parent.
    #[test]
    fn a_class_rule_also_matches_anything_that_refines_it() {
        let reg = registry_with(&["m"]);
        let rule = rule_with(
            RuleConditions {
                class: ClassMatch {
                    class: Some("coding".into()),
                },
                ..Default::default()
            },
            "m",
        );
        let headers = HeaderMap::new();
        let mut facts = facts_with(None, 0, None, &headers);
        facts.class = Some("debugging");
        let refines = vec!["coding".to_string()];
        facts.class_refines = &refines;
        assert!(rule.matches(&facts, &reg));
    }

    // --- default fallback -------------------------------------------

    #[test]
    fn no_matching_rule_falls_back_to_the_virtual_models_defaults() {
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![RoutingRule {
                conditions: RuleConditions {
                    caller: CallerMatch {
                        principals: [999].into_iter().collect(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                targets: vec![target("only-for-999", 1)],
            }],
            default_targets: vec![target("default-model", 1)],
        };
        let reg = registry_with(&["only-for-999", "default-model"]);
        assert_eq!(
            vm.resolve(
                &facts_with(Some(&principal(1, &[])), 0, None, &HeaderMap::new()),
                0,
                &reg
            )
            .as_deref(),
            Some("default-model")
        );
    }

    #[test]
    fn a_matched_rule_with_no_routable_target_does_not_fall_through_to_defaults() {
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![RoutingRule {
                conditions: RuleConditions::default(),
                targets: vec![],
            }],
            default_targets: vec![target("default-model", 1)],
        };
        let reg = registry_with(&["default-model"]);
        assert_eq!(
            vm.resolve(&facts_with(None, 0, None, &HeaderMap::new()), 0, &reg),
            None,
            "the matching rule's own (empty) targets decide; defaults are only \
             for 'nothing matched', not 'what matched had nothing to offer'"
        );
    }

    // --- estimation ---------------------------------------------------

    #[test]
    fn prompt_token_estimate_is_approximate_bytes_over_the_documented_ratio() {
        assert_eq!(estimate_prompt_tokens(0), 0);
        // 350 bytes / 3.5 bytes-per-token = 100, chosen to land exactly.
        assert_eq!(estimate_prompt_tokens(350), 100);
    }
}
