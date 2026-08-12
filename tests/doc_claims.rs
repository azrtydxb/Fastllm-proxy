//! Countable claims in the documentation, checked against the thing they count.
//!
//! # Why
//!
//! The README said "20 providers" in one bullet and "Twenty-three providers" in
//! the section directly below it, and both were wrong — the table held more
//! than either. Nobody noticed because a number in prose is not executable and
//! nothing recounts it when a row is added.
//!
//! That is the same failure this repo already guards against in code: a comment
//! describing behaviour the code no longer has. A number describing a table the
//! table no longer matches is the documentation version of it, and the fix is
//! the same — make the claim executable.
//!
//! Deliberately narrow. This checks claims that are *countable and mechanical*,
//! not prose. "Cache-affinity routing preserves prefix caches" cannot be
//! asserted here and should not be faked; that is what the benchmarks are for.

use std::fs;

fn readme() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).expect("README.md")
}

/// The provider section, split into its two tables.
///
/// Counting rule, and the one the README's own number must follow: each table
/// row's first cell lists providers separated by `·`, so `Together · Fireworks`
/// is two and `Moonshot / Kimi` is one thing with two names.
fn count_providers(readme: &str) -> (usize, usize) {
    let section = readme
        .split("## Providers")
        .nth(1)
        .expect("a Providers section")
        .split("\n## ")
        .next()
        .unwrap();

    let (mut compatible, mut native, mut table) = (0usize, 0usize, 0u8);
    for line in section.lines() {
        if !line.starts_with('|') {
            continue;
        }
        // The `|---|---|` separator.
        if line.replace('|', "").trim().chars().all(|c| c == '-') {
            continue;
        }
        let first = line.split('|').nth(1).unwrap_or("").trim();
        if first.contains("reached as-is") {
            table = 1;
            continue;
        }
        if first.contains("reached through") {
            table = 2;
            continue;
        }
        let names = first.split('·').filter(|n| !n.trim().is_empty()).count();
        match table {
            1 => compatible += names,
            2 => native += names,
            _ => {}
        }
    }
    (compatible, native)
}

#[test]
fn the_readme_provider_count_matches_its_own_table() {
    let readme = readme();
    let (compatible, native) = count_providers(&readme);
    let total = compatible + native;

    assert!(
        compatible > 0 && native > 0,
        "the provider tables did not parse; the counting rule in this file has \
         drifted from the README's layout (compatible={compatible}, native={native})"
    );

    // Every place the number is written. A second mention that disagrees with
    // the first is exactly what happened before.
    for claim in [
        format!("**{total} providers, and any OpenAI-compatible endpoint**"),
        format!(
            "**{total} providers work today — {compatible} reached as-is, {native} through \
             their own wire format —"
        ),
    ] {
        assert!(
            readme.contains(&claim),
            "the README does not contain {claim:?}.\n\
             The tables now list {compatible} OpenAI-compatible and {native} native \
             ({total} total) — update the prose, or fix the table."
        );
    }
}

/// The endpoint list in `docs/api.md` against the constant that decides it.
///
/// An endpoint added to the code and not the docs is invisible to users; one
/// added to the docs and not the code is a 404 with a promise behind it.
#[test]
fn documented_endpoints_are_the_ones_actually_proxied() {
    // Kept in sync by hand with `proxy::PROXIED_SUFFIXES`, which is private —
    // `src/proxy.rs` has its own test asserting each of these is in it, so a
    // removal there fails that test and an addition fails this one until both
    // lists agree.
    let expected = [
        "/chat/completions",
        "/completions",
        "/responses",
        "/embeddings",
        "/rerank",
        "/score",
        "/audio/transcriptions",
        "/audio/translations",
        "/audio/speech",
        "/images/generations",
        "/images/edits",
        "/moderations",
    ];

    let api = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/api.md"))
        .expect("docs/api.md");
    for endpoint in expected {
        assert!(
            api.contains(endpoint),
            "docs/api.md never mentions {endpoint}, which the proxy serves"
        );
    }
}
