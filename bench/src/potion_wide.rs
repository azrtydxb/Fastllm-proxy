//! Which classes become viable once a second tier exists?
//!
//! The static model gives five reliable classes and fails on architecture. A
//! transformer tier fixes architecture (93.3% precision against 75.3%). The
//! question this asks is how far that generalises: which *other* classes are
//! worth offering, on which tier, and where the ceiling is.
//!
//! Two families of candidate:
//!
//! - **Domains** — security, databases, data science, statistics, law, finance,
//!   UX, writing, devops. Each is its own StackExchange community, so the
//!   boundaries were drawn by the people asking rather than by us.
//! - **The blurry region** — summarise / rewrite / extract / classify, which
//!   collapsed under the static model at centroid similarities of 0.81-0.86.
//!   These differ by instruction verb rather than subject, which is exactly
//!   what a contextual model should recover and a bag of tokens cannot.
//!
//! Both models are run over both families, so every candidate class gets a
//! tier recommendation rather than a yes/no.
//!
//! ```text
//! python3 bench/fetch-prompts.py
//! cargo run -p bench --release --bin potion-wide
//! ```

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use model2vec_rs::model::StaticModel;
use std::collections::HashMap;

const SEEDS: usize = 40;
const CAP: usize = 128;

#[derive(serde::Deserialize, Clone)]
struct Labelled {
    prompt: String,
    #[allow(dead_code)]
    category: String,
}

fn load(name: &str) -> Vec<Labelled> {
    let path = format!("{}/data/{name}", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}\nrun `python3 bench/fetch-prompts.py` first"));
    serde_json::from_str(&raw).expect("dataset is JSON")
}

fn normalise(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn mean(vectors: &[&Vec<f32>]) -> Vec<f32> {
    let mut sum = vec![0.0f32; vectors[0].len()];
    for v in vectors {
        for (s, x) in sum.iter_mut().zip(v.iter()) {
            *s += x;
        }
    }
    for s in sum.iter_mut() {
        *s /= vectors.len() as f32;
    }
    normalise(&mut sum);
    sum
}

fn clip(s: &str) -> String {
    s.chars().take(600).collect()
}

type Named = (String, Vec<Vec<f32>>);

/// Per-class precision/recall at a floor, plus overall coverage and accuracy.
fn score(sets: &[Named], floor: f32) -> (f64, f64, Vec<(String, f64, f64)>) {
    let centroids: Vec<(&str, Vec<f32>)> = sets
        .iter()
        .map(|(n, v)| {
            (
                n.as_str(),
                mean(&v[..SEEDS.min(v.len() / 2)].iter().collect::<Vec<_>>()),
            )
        })
        .collect();
    let mut tp: HashMap<&str, usize> = HashMap::new();
    let mut pred: HashMap<&str, usize> = HashMap::new();
    let mut act: HashMap<&str, usize> = HashMap::new();
    let (mut kept, mut total) = (0usize, 0usize);
    for (name, vectors) in sets {
        for v in &vectors[SEEDS.min(vectors.len() / 2)..] {
            total += 1;
            let mut scored: Vec<(&str, f32)> =
                centroids.iter().map(|(n, c)| (*n, cosine(v, c))).collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            if scored[0].1 - scored[1].1 < floor {
                continue;
            }
            kept += 1;
            *pred.entry(scored[0].0).or_default() += 1;
            *act.entry(name.as_str()).or_default() += 1;
            if scored[0].0 == name.as_str() {
                *tp.entry(scored[0].0).or_default() += 1;
            }
        }
    }
    let correct: usize = tp.values().sum();
    let per_class = sets
        .iter()
        .map(|(n, _)| {
            let t = *tp.get(n.as_str()).unwrap_or(&0) as f64;
            let p = *pred.get(n.as_str()).unwrap_or(&0) as f64;
            let a = *act.get(n.as_str()).unwrap_or(&0) as f64;
            (
                n.clone(),
                if p > 0.0 { t / p } else { 0.0 },
                if a > 0.0 { t / a } else { 0.0 },
            )
        })
        .collect();
    (
        kept as f64 / total.max(1) as f64,
        correct as f64 / kept.max(1) as f64,
        per_class,
    )
}

fn report(title: &str, sets: &[Named], floors: &[f32]) {
    println!("\n--- {title} ({} classes) ---", sets.len());
    for floor in floors {
        let (coverage, accuracy, per_class) = score(sets, *floor);
        println!(
            "  floor {floor:.2}: coverage {:.0}%  accuracy {:.1}%",
            coverage * 100.0,
            accuracy * 100.0
        );
        if (*floor - floors[floors.len() / 2]).abs() < f32::EPSILON {
            let mut rows = per_class;
            rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            for (n, p, r) in rows {
                let verdict = if p >= 0.85 {
                    "good"
                } else if p >= 0.70 {
                    "usable"
                } else {
                    "weak"
                };
                println!(
                    "      {n:<16} precision {:>5.1}%  recall {:>5.1}%   {verdict}",
                    p * 100.0,
                    r * 100.0
                );
            }
        }
    }
}

fn main() {
    let domains: Vec<(&str, &str)> = vec![
        ("architecture", "se_architecture.json"),
        ("code-review", "se_codereview.json"),
        ("devops", "se_devops.json"),
        ("security", "se_security.json"),
        ("databases", "se_dba.json"),
        ("data-science", "se_datascience.json"),
        ("statistics", "se_stats.json"),
        ("legal", "se_law.json"),
        ("finance", "se_money.json"),
        ("ux-design", "se_ux.json"),
        ("writing-craft", "se_writers.json"),
    ];
    let domain_rows: Vec<(&str, Vec<Labelled>)> =
        domains.iter().map(|(n, f)| (*n, load(f))).collect();

    // The classes the static model collapsed. They differ by instruction verb,
    // not by subject, which is the specific thing a contextual model can see.
    let no_robots = load("no_robots.json");
    let blurry: Vec<(&str, Vec<Labelled>)> = ["Summarize", "Rewrite", "Extract", "Classify"]
        .iter()
        .map(|c| {
            (
                *c,
                no_robots
                    .iter()
                    .filter(|r| r.category == *c)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        })
        .collect();

    // --- tier 1: the static model ------------------------------------
    println!("################ tier 1: potion-code-16M (~0.14ms) ################");
    let statik = StaticModel::from_pretrained("minishlab/potion-code-16M", None, None, None)
        .expect("load potion");
    let embed_static = |rows: &[Labelled]| -> Vec<Vec<f32>> {
        let texts: Vec<String> = rows.iter().map(|r| r.prompt.clone()).collect();
        let mut v = statik.encode_with_args(&texts, Some(CAP), 1024);
        for x in v.iter_mut() {
            normalise(x);
        }
        v
    };

    let domain_sets_1: Vec<Named> = domain_rows
        .iter()
        .map(|(n, rows)| (n.to_string(), embed_static(rows)))
        .collect();
    report(
        "eleven technical/professional domains",
        &domain_sets_1,
        &[0.0, 0.05, 0.10],
    );

    let blurry_sets_1: Vec<Named> = blurry
        .iter()
        .map(|(n, rows)| (n.to_string(), embed_static(rows)))
        .collect();
    report(
        "the blurry region (verb-distinguished)",
        &blurry_sets_1,
        &[0.0, 0.05, 0.10],
    );

    // --- tier 2: the transformer -------------------------------------
    println!("\n################ tier 2: bge-small-en-v1.5 (~3.3ms) ################");
    let mut bge =
        TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15)).expect("load bge");
    let mut embed_bge = |rows: &[Labelled]| -> Vec<Vec<f32>> {
        let texts: Vec<String> = rows.iter().map(|r| clip(&r.prompt)).collect();
        let mut v = bge.embed(texts, Some(256)).expect("embed");
        for x in v.iter_mut() {
            normalise(x);
        }
        v
    };

    let domain_sets_2: Vec<Named> = domain_rows
        .iter()
        .map(|(n, rows)| (n.to_string(), embed_bge(rows)))
        .collect();
    // Floors differ from tier 1 on purpose: bge-small's space is anisotropic,
    // so its margins are compressed and a floor calibrated on potion would
    // reject nearly everything. Floors are per model, always.
    report(
        "eleven technical/professional domains",
        &domain_sets_2,
        &[0.0, 0.02, 0.05],
    );

    let blurry_sets_2: Vec<Named> = blurry
        .iter()
        .map(|(n, rows)| (n.to_string(), embed_bge(rows)))
        .collect();
    report(
        "the blurry region (verb-distinguished)",
        &blurry_sets_2,
        &[0.0, 0.02, 0.05],
    );
}
