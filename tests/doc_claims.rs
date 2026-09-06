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

/// The catalogue itself, which lives in the book rather than the README.
///
/// It moved because it was the largest thing on a front page whose job is to
/// make someone want to read further, and because a table that long wants a
/// page of its own with the "how do I add one" material beside it. The claim
/// stayed in the README. That split is exactly what this test is for: prose in
/// one file, the rows it counts in another, and nothing but this to notice
/// when they stop agreeing.
fn providers_page() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/providers.md"))
        .expect("docs/providers.md")
}

/// The provider section, split into its two tables.
///
/// Counting rule, and the one the README's own number must follow: each table
/// row's first cell lists providers separated by `·`, so `Together · Fireworks`
/// is two and `Moonshot / Kimi` is one thing with two names.
fn count_providers(page: &str) -> (usize, usize) {
    let section = page
        .split("## The catalogue")
        .nth(1)
        .expect("a catalogue section")
        .split("\n## ")
        .next()
        .unwrap();

    let (mut compatible, mut native, mut table) = (0usize, 0usize, 0u8);
    for line in section.lines() {
        if !line.starts_with('|') {
            continue;
        }
        // The separator row — `|---|---|`, or `| --- | --- |` as prettier
        // writes it, with `:` for alignment markers.
        if line.chars().all(|c| matches!(c, '|' | '-' | ' ' | ':')) {
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
fn the_readme_provider_count_matches_the_catalogue_page() {
    let readme = readme();
    let (compatible, native) = count_providers(&providers_page());
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

/// The endpoint list in `docs/api/endpoints.md` against the constant that decides it.
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

    // The page, not the chapter: `docs/api.md` is now an index, and the
    // endpoint list lives under it. A test that reads the index would pass on
    // a table of contents that mentions everything and documents nothing.
    let api = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/api/endpoints.md"
    ))
    .expect("docs/api/endpoints.md");
    for endpoint in expected {
        assert!(
            api.contains(endpoint),
            "docs/api/endpoints.md never mentions {endpoint}, which the proxy serves"
        );
    }
}

/// Every provider `docs/providers.md` names has a catalogue entry.
///
/// The catalogue used to hold fourteen of the eighty the page names — the ones
/// somebody had got round to typing an address for — which made the Add
/// provider dropdown read as the list of what FastLLM supports rather than the
/// list of what had been seeded. This is the guard that keeps the two together
/// as the page grows: seeding is a migration, and a migration is easy to
/// forget.
///
/// It counts, rather than matching name for name. The page writes a provider
/// the way a human says it ("Moonshot / Kimi", "Weights & Biases") and the
/// catalogue writes a stable key, so pinning the mapping here would make
/// renaming a row in either place a test failure with nothing wrong.
#[test]
fn the_catalogue_covers_every_provider_the_page_names() {
    let page = providers_page();
    let (compatible, native) = count_providers(&page);
    let named = compatible + native;

    // Both seeding migrations, since 0042 adds to what 0039 started.
    let mut seeded = 0usize;
    for entry in fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")).unwrap() {
        let path = entry.unwrap().path();
        let sql = fs::read_to_string(&path).unwrap_or_default();
        if !sql.contains("INSERT INTO provider_catalogue") {
            continue;
        }
        // One row per line, each opening with the key in single quotes.
        seeded += sql
            .lines()
            .filter(|l| l.trim_start().starts_with("('"))
            .count();
    }

    assert!(
        seeded >= named,
        "docs/providers.md names {named} providers but only {seeded} are seeded into \
         provider_catalogue; add the missing ones in a new migration so the Add provider \
         dropdown offers what the page promises"
    );
}
