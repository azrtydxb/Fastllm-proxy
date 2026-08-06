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
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

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
}

impl ShapeMatch {
    fn matches(&self, estimated_prompt_tokens: u64, requested_max_tokens: Option<u64>) -> bool {
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
}

impl MatchConditionJson {
    pub fn into_caller_and_shape(self) -> (CallerMatch, ShapeMatch) {
        (
            CallerMatch {
                principals: self.principals.into_iter().collect(),
                roles: self.roles.into_iter().collect(),
            },
            ShapeMatch {
                min_prompt_tokens: self.min_prompt_tokens,
                max_prompt_tokens: self.max_prompt_tokens,
                min_max_tokens: self.min_max_tokens,
                max_max_tokens: self.max_max_tokens,
            },
        )
    }
}

/// One rule in a virtual model's ordered chain: match conditions AND'd
/// together, and the targets to route to when they all hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingRule {
    pub caller: CallerMatch,
    pub shape: ShapeMatch,
    pub targets: Vec<WeightedTarget>,
}

impl RoutingRule {
    fn matches(
        &self,
        caller: Option<&Principal>,
        prompt_tokens: u64,
        max_tokens: Option<u64>,
    ) -> bool {
        self.caller.matches(caller) && self.shape.matches(prompt_tokens, max_tokens)
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
        caller: Option<&Principal>,
        prompt_tokens: u64,
        max_tokens: Option<u64>,
        prefix_hash: u64,
        registry: &Registry,
    ) -> Option<String> {
        for rule in &self.rules {
            if rule.matches(caller, prompt_tokens, max_tokens) {
                return select_with_failover(&rule.targets, prefix_hash, registry);
            }
        }
        select_with_failover(&self.default_targets, prefix_hash, registry)
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
fn select_with_failover(
    targets: &[WeightedTarget],
    prefix_hash: u64,
    registry: &Registry,
) -> Option<String> {
    let chosen = choose_weighted(targets, prefix_hash)?;
    if registry.pool_has_healthy(&chosen.model) {
        return Some(chosen.model.clone());
    }
    for t in targets {
        if t.model != chosen.model && registry.pool_has_healthy(&t.model) {
            return Some(t.model.clone());
        }
    }
    Some(chosen.model.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileConfig;
    use crate::registry::Interner;
    use crate::snapshot::Principal;
    use std::collections::HashSet as Set;

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
        }
    }

    // --- rule ordering -----------------------------------------------

    #[test]
    fn the_first_matching_rule_wins() {
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![
                RoutingRule {
                    targets: vec![target("first", 1)],
                    ..Default::default()
                },
                RoutingRule {
                    targets: vec![target("second", 1)],
                    ..Default::default()
                },
            ],
            default_targets: vec![target("default", 1)],
        };
        let reg = registry_with(&["first", "second", "default"]);
        assert_eq!(
            vm.resolve(None, 0, None, 0, &reg).as_deref(),
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
                    caller: CallerMatch {
                        principals: [999].into_iter().collect(),
                        ..Default::default()
                    },
                    targets: vec![target("only-for-999", 1)],
                    ..Default::default()
                },
                RoutingRule {
                    targets: vec![target("catch-all", 1)],
                    ..Default::default()
                },
            ],
            default_targets: vec![],
        };
        let reg = registry_with(&["only-for-999", "catch-all"]);
        let caller = principal(1, &[]);
        assert_eq!(
            vm.resolve(Some(&caller), 0, None, 0, &reg).as_deref(),
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
        assert!(m.matches(50, None));
        assert!(!m.matches(101, None));
    }

    #[test]
    fn shape_match_on_requested_max_tokens() {
        let m = ShapeMatch {
            max_max_tokens: Some(256),
            ..Default::default()
        };
        assert!(m.matches(0, Some(100)));
        assert!(!m.matches(0, Some(257)));
        assert!(
            !m.matches(0, None),
            "a bound on max_tokens cannot be satisfied by a request that omitted it"
        );
    }

    #[test]
    fn combined_caller_and_shape_conditions_both_must_hold() {
        let rule = RoutingRule {
            caller: CallerMatch {
                roles: ["canary".to_string()].into_iter().collect(),
                ..Default::default()
            },
            shape: ShapeMatch {
                max_prompt_tokens: Some(1000),
                ..Default::default()
            },
            targets: vec![target("m", 1)],
        };
        let canary = principal(1, &["canary"]);
        let other = principal(2, &["other"]);
        assert!(rule.matches(Some(&canary), 500, None), "role ok, shape ok");
        assert!(
            !rule.matches(Some(&other), 500, None),
            "shape ok but wrong role"
        );
        assert!(
            !rule.matches(Some(&canary), 5000, None),
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
        let first = vm.resolve(None, 0, None, prefix, &reg);
        for _ in 0..200 {
            assert_eq!(
                vm.resolve(None, 0, None, prefix, &reg),
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
            match vm.resolve(None, 0, None, prefix, &reg).as_deref() {
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
            vm.resolve(None, 0, None, 0, &reg).as_deref(),
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
            vm.resolve(None, 0, None, 0, &reg).as_deref(),
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
            vm.resolve(None, 0, None, 0, &reg).as_deref(),
            Some("primary"),
            "nothing healthy: fall back to the weighted pick rather than None, \
             same as Router::pick's last-resort rule"
        );
    }

    // --- default fallback -------------------------------------------

    #[test]
    fn no_matching_rule_falls_back_to_the_virtual_models_defaults() {
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![RoutingRule {
                caller: CallerMatch {
                    principals: [999].into_iter().collect(),
                    ..Default::default()
                },
                targets: vec![target("only-for-999", 1)],
                ..Default::default()
            }],
            default_targets: vec![target("default-model", 1)],
        };
        let reg = registry_with(&["only-for-999", "default-model"]);
        assert_eq!(
            vm.resolve(Some(&principal(1, &[])), 0, None, 0, &reg)
                .as_deref(),
            Some("default-model")
        );
    }

    #[test]
    fn a_matched_rule_with_no_routable_target_does_not_fall_through_to_defaults() {
        let vm = VirtualModelDef {
            name: "vm".into(),
            rules: vec![RoutingRule {
                targets: vec![],
                ..Default::default()
            }],
            default_targets: vec![target("default-model", 1)],
        };
        let reg = registry_with(&["default-model"]);
        assert_eq!(
            vm.resolve(None, 0, None, 0, &reg),
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
