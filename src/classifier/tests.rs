//! Dispatch logic, tested without a model.
//!
//! Centroids here are hand-written unit vectors, so these assert the thing that
//! is actually easy to get wrong — when tier 2 runs, which classes it may
//! choose between, and what happens when it declines — rather than re-measuring
//! embedding quality. Model quality is measured in `bench/potion-real` against
//! ~21k labelled prompts; a test suite is the wrong instrument for it and would
//! only be slow and flaky.

use super::*;

/// Axis-aligned unit vector, so cosine similarity against another axis is 0 and
/// against itself is 1 — margins become arithmetic a reader can check by eye.
fn axis(dim: usize, i: usize) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    v[i] = 1.0;
    v
}

/// Blend two axes, for a vector that sits deliberately between two classes.
fn between(dim: usize, a: usize, b: usize, bias: f32) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    v[a] = bias;
    v[b] = 1.0 - bias;
    normalise(&mut v);
    v
}

fn class(
    name: &str,
    tier: Tier,
    centroid: Vec<f32>,
    min_margin: f32,
    refines: &[&str],
) -> PromptClass {
    PromptClass {
        name: name.into(),
        tier,
        centroid: Some(centroid),
        min_margin,
        refines: refines.iter().map(|s| s.to_string()).collect(),
    }
}

fn fast_only() -> Classifier {
    Classifier::new(vec![
        class("coding", Tier::Fast, axis(4, 0), 0.05, &[]),
        class("chat", Tier::Fast, axis(4, 1), 0.05, &[]),
    ])
}

/// The property the whole design rests on: with no tier-2 class configured,
/// nothing can reach the transformer, so a deployment that routes on subject
/// alone never pays for one.
#[test]
fn tier2_is_unreachable_until_a_refined_class_exists() {
    let c = fast_only();
    assert!(!c.tier2_reachable());

    let mut called = false;
    let got = c.classify(&axis(4, 0), || {
        called = true;
        None
    });
    assert_eq!(got.map(|g| g.class), Some("coding".into()));
    assert!(!called, "the refined embedder must not be invoked at all");
}

#[test]
fn a_refined_class_only_opens_the_door_for_the_classes_it_names() {
    let c = Classifier::new(vec![
        class("coding", Tier::Fast, axis(4, 0), 0.05, &[]),
        class("chat", Tier::Fast, axis(4, 1), 0.05, &[]),
        class("architecture", Tier::Refined, axis(4, 2), 0.05, &["coding"]),
    ]);
    assert!(c.tier2_reachable());

    // A chat prompt is not something architecture competes for, so it settles
    // on the cheap tier even though tier 2 exists.
    let mut called = false;
    let got = c.classify(&axis(4, 1), || {
        called = true;
        None
    });
    assert_eq!(got.unwrap().class, "chat");
    assert!(!called, "chat is not in any refined class's `refines` list");

    // A coding prompt is — but a lone refined class has nothing to be compared
    // against, so it does not escalate either. See the two-contender test below.
    let mut called = false;
    let got = c.classify(&axis(4, 0), || {
        called = true;
        Some(axis(4, 2))
    });
    assert!(
        !called,
        "one refined contender is not a choice; do not escalate"
    );
    assert_eq!(got.unwrap().class, "coding");
}

/// The bug this guards: with a single refined contender there is no runner-up,
/// so the margin degenerates to a raw similarity score, a margin-shaped floor
/// like 0.10 is met by almost anything, and that one class silently captures
/// every request the fast tier assigned to the class it refines.
#[test]
fn a_lone_refined_class_never_captures_the_class_it_refines() {
    let c = Classifier::new(vec![
        class("coding", Tier::Fast, axis(4, 0), 0.05, &[]),
        class("architecture", Tier::Refined, axis(4, 2), 0.10, &["coding"]),
    ]);
    // The refined embedding points straight at `architecture`: raw similarity
    // 1.0, which would clear any margin-shaped floor.
    let got = c.classify(&axis(4, 0), || Some(axis(4, 2))).unwrap();
    assert_eq!(
        got.class, "coding",
        "a refinement with nothing to choose between must not overturn tier 1"
    );
}

/// The configuration that works, and the one the measurements were taken on: a
/// refined class per side of the distinction, both refining the same fast class,
/// so tier 2 answers a binary question.
#[test]
fn tier2_can_overturn_tier1_when_there_are_two_sides_to_choose_between() {
    let c = Classifier::new(vec![
        class("coding", Tier::Fast, axis(4, 0), 0.05, &[]),
        class("chat", Tier::Fast, axis(4, 1), 0.05, &[]),
        class("architecture", Tier::Refined, axis(4, 2), 0.05, &["coding"]),
        class("debugging", Tier::Refined, axis(4, 3), 0.05, &["coding"]),
    ]);
    let got = c.classify(&axis(4, 0), || Some(axis(4, 2))).unwrap();
    assert_eq!(got.class, "architecture");
    assert_eq!(got.tier, Tier::Refined);

    // And the other side wins when the prompt leans that way.
    let got = c.classify(&axis(4, 0), || Some(axis(4, 3))).unwrap();
    assert_eq!(got.class, "debugging");
}

/// The common shape, and the one that keeps escalation cheap in practice: most
/// coding prompts really are coding, tier 2 declines to commit, and tier 1's
/// answer stands.
#[test]
fn tier1s_answer_stands_when_tier2_is_below_its_floor() {
    let c = Classifier::new(vec![
        class("coding", Tier::Fast, axis(4, 0), 0.05, &[]),
        class("chat", Tier::Fast, axis(4, 1), 0.05, &[]),
        // Two refined classes so there is a runner-up to have a margin against.
        class("architecture", Tier::Refined, axis(4, 2), 0.9, &["coding"]),
        class("ops", Tier::Refined, axis(4, 3), 0.9, &["coding"]),
    ]);
    // Sits between the two refined classes, so neither clears a 0.9 floor.
    let got = c
        .classify(&axis(4, 0), || Some(between(4, 2, 3, 0.5)))
        .unwrap();
    assert_eq!(got.class, "coding");
    assert_eq!(got.tier, Tier::Fast);
}

/// A classifier is a routing aid, not a gate. If the transformer is missing or
/// failed to load, the request still routes on tier 1's answer.
#[test]
fn an_unavailable_tier2_degrades_rather_than_failing() {
    let c = Classifier::new(vec![
        class("coding", Tier::Fast, axis(4, 0), 0.05, &[]),
        class("chat", Tier::Fast, axis(4, 1), 0.05, &[]),
        class("architecture", Tier::Refined, axis(4, 2), 0.05, &["coding"]),
        class("debugging", Tier::Refined, axis(4, 3), 0.05, &["coding"]),
    ]);
    let got = c.classify(&axis(4, 0), || None).unwrap();
    assert_eq!(got.class, "coding");
}

/// Below the floor is a routing decision — the rule does not match and the next
/// rule catches it — not an error and not a guess.
#[test]
fn a_prompt_between_two_classes_is_left_unclassified() {
    let c = Classifier::new(vec![
        class("coding", Tier::Fast, axis(4, 0), 0.2, &[]),
        class("chat", Tier::Fast, axis(4, 1), 0.2, &[]),
    ]);
    assert!(c.classify(&between(4, 0, 1, 0.5), || None).is_none());
}

/// Floors are per class because measured precision varies from 98% to 35%
/// across classes an operator might define; one threshold cannot serve both.
#[test]
fn each_class_is_held_to_its_own_floor() {
    let c = Classifier::new(vec![
        class("strict", Tier::Fast, axis(4, 0), 0.95, &[]),
        class("lenient", Tier::Fast, axis(4, 1), 0.01, &[]),
    ]);
    // Leans to `strict` but nowhere near its floor.
    assert!(c.classify(&between(4, 0, 1, 0.6), || None).is_none());
    // The same lean toward `lenient` clears its much lower floor.
    let got = c.classify(&between(4, 1, 0, 0.6), || None).unwrap();
    assert_eq!(got.class, "lenient");
}

/// A class whose examples could not be embedded has no centroid. It must drop
/// out of routing entirely rather than matching everything or nothing at some
/// arbitrary distance.
#[test]
fn a_class_without_a_centroid_is_ignored() {
    let c = Classifier::new(vec![
        class("coding", Tier::Fast, axis(4, 0), 0.05, &[]),
        PromptClass {
            name: "broken".into(),
            tier: Tier::Fast,
            centroid: None,
            min_margin: 0.0,
            refines: vec![],
        },
    ]);
    let got = c.classify(&axis(4, 0), || None).unwrap();
    assert_eq!(got.class, "coding");
}

/// `refines` naming a class that no longer exists is untidy configuration, not
/// an outage: the operator may have deleted the tier-1 class and not yet
/// tidied the reference.
#[test]
fn refining_an_unknown_class_is_inert() {
    let c = Classifier::new(vec![
        class("coding", Tier::Fast, axis(4, 0), 0.05, &[]),
        class("chat", Tier::Fast, axis(4, 1), 0.05, &[]),
        class(
            "architecture",
            Tier::Refined,
            axis(4, 2),
            0.05,
            &["deleted-class"],
        ),
    ]);
    assert!(
        !c.tier2_reachable(),
        "a dangling refines target must not open the expensive path"
    );
}

#[test]
fn centroid_is_the_normalised_mean() {
    let c = centroid(&[axis(3, 0), axis(3, 1)]).unwrap();
    let norm: f32 = c.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-5, "centroids are unit length");
    assert!(
        (c[0] - c[1]).abs() < 1e-6,
        "equal inputs, equal contribution"
    );
    assert_eq!(c[2], 0.0);
}

#[test]
fn cosine_of_mismatched_dimensions_never_wins() {
    // Guards against a snapshot built by one model being scored against another
    // — the result must lose every comparison rather than silently ranking.
    assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), f32::NEG_INFINITY);
}

#[test]
fn tier_names_round_trip() {
    for t in [Tier::Fast, Tier::Refined] {
        assert_eq!(Tier::parse(t.as_str()), Some(t));
    }
    assert_eq!(Tier::parse("gpu"), None);
}
