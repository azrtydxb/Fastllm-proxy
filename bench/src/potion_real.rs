//! Model2Vec routing classifiers, evaluated on real labelled data.
//!
//! The sibling `potion` benchmark used sixty prompts written by whoever wrote
//! the benchmark, which measures the author's imagination as much as the model.
//! This one uses `HuggingFaceH4/no_robots` — ~9.5k prompts written by people and
//! labelled by people, neither of whom knew about this proxy — plus `openai/gsm8k`
//! for the one question no dataset labels directly.
//!
//! Run `python3 bench/fetch-prompts.py` first; it caches into `bench/data/`.
//!
//! ```text
//! cargo run -p bench --release --bin potion-real
//! ```
//!
//! Three experiments, in increasing order of how much they should change the
//! design:
//!
//! 1. **Coding vs the rest.** The single most valuable routing decision, and a
//!    binary one, so precision and recall are separable and a threshold means
//!    something.
//! 2. **The full ten-way category problem.** Realistically hard, and mostly a
//!    check on whether the embedding carries topic structure at all.
//! 3. **Reasoning vs lookup.** GSM8K word problems against no_robots' factual
//!    "Open QA". This is the difficulty question — "does this need the expensive
//!    model" — and it is the one I expect a static embedding to fail, because
//!    difficulty is not a vocabulary property. Better to find out here than in
//!    production.

use model2vec_rs::model::StaticModel;
use std::collections::HashMap;
use std::time::Instant;

const MODELS: &[(&str, &str)] = &[
    ("potion-base-2M", "minishlab/potion-base-2M"),
    ("potion-base-8M", "minishlab/potion-base-8M"),
    ("potion-code-16M", "minishlab/potion-code-16M"),
    ("potion-retrieval-32M", "minishlab/potion-retrieval-32M"),
];

/// Token cap. 32 was as accurate as 512 in the synthetic sweep and ~12x
/// cheaper; this run re-checks that on real prompts, where the first line is
/// less reliably the whole intent.
const CAPS: &[usize] = &[32, 128];

#[derive(serde::Deserialize, Clone)]
struct Labelled {
    prompt: String,
    category: String,
}

fn load(name: &str) -> Vec<Labelled> {
    let path = format!("{}/data/{name}", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}\nrun `python3 bench/fetch-prompts.py` first"));
    serde_json::from_str(&raw).expect("dataset is JSON")
}

// ---------------------------------------------------------------------------
// Vectors
// ---------------------------------------------------------------------------

fn normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn centroid(vectors: &[&Vec<f32>]) -> Vec<f32> {
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

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Deterministic shuffle. Not `rand`: a benchmark whose split changes run to
/// run cannot be compared against its own previous output, and the whole point
/// of this file is to compare models against each other.
fn shuffled<T: Clone>(items: &[T], seed: u64) -> Vec<T> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    let mut state = seed | 1;
    for i in (1..idx.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        idx.swap(i, (state % (i as u64 + 1)) as usize);
    }
    idx.into_iter().map(|i| items[i].clone()).collect()
}

fn embed(model: &StaticModel, texts: &[String], cap: usize) -> Vec<Vec<f32>> {
    let mut v = model.encode_with_args(texts, Some(cap), 1024);
    for x in v.iter_mut() {
        normalise(x);
    }
    v
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

struct ClassReport {
    name: String,
    precision: f64,
    recall: f64,
    support: usize,
}

struct Report {
    accuracy: f64,
    macro_f1: f64,
    classes: Vec<ClassReport>,
    /// (margin floor, share of traffic classified, accuracy on that share)
    coverage: Vec<(f32, f64, f64)>,
}

/// Nearest-centroid classification with a held-out test split.
///
/// Centroids come from the training half only. That matters more than it might
/// look: scoring against centroids that contain the test prompt inflates every
/// number, and at this sample size it would inflate them enough to change which
/// model looks best.
fn evaluate(model: &StaticModel, data: &[Labelled], cap: usize, train_frac: f64) -> Report {
    let shuffled = shuffled(data, 0x5eed_1234);
    let split = (shuffled.len() as f64 * train_frac) as usize;
    let (train, test) = shuffled.split_at(split);

    let train_texts: Vec<String> = train.iter().map(|r| r.prompt.clone()).collect();
    let test_texts: Vec<String> = test.iter().map(|r| r.prompt.clone()).collect();
    let train_vecs = embed(model, &train_texts, cap);
    let test_vecs = embed(model, &test_texts, cap);

    let mut by_class: HashMap<&str, Vec<&Vec<f32>>> = HashMap::new();
    for (row, v) in train.iter().zip(&train_vecs) {
        by_class.entry(row.category.as_str()).or_default().push(v);
    }
    let mut names: Vec<&str> = by_class.keys().copied().collect();
    names.sort();
    let centroids: Vec<(&str, Vec<f32>)> =
        names.iter().map(|n| (*n, centroid(&by_class[n]))).collect();

    let mut tp: HashMap<&str, usize> = HashMap::new();
    let mut predicted_count: HashMap<&str, usize> = HashMap::new();
    let mut actual_count: HashMap<&str, usize> = HashMap::new();
    // (margin, was_correct) for the coverage curve.
    let mut decisions: Vec<(f32, bool)> = Vec::with_capacity(test.len());

    for (row, v) in test.iter().zip(&test_vecs) {
        let mut scored: Vec<(&str, f32)> =
            centroids.iter().map(|(n, c)| (*n, cosine(v, c))).collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let correct = scored[0].0 == row.category;
        let margin = scored[0].1 - scored.get(1).map(|s| s.1).unwrap_or(0.0);
        decisions.push((margin, correct));
        *predicted_count.entry(scored[0].0).or_default() += 1;
        *actual_count
            .entry(
                names
                    .iter()
                    .find(|n| **n == row.category)
                    .copied()
                    .unwrap_or("?"),
            )
            .or_default() += 1;
        if correct {
            *tp.entry(scored[0].0).or_default() += 1;
        }
    }

    let accuracy = decisions.iter().filter(|(_, ok)| *ok).count() as f64 / decisions.len() as f64;

    let mut classes = Vec::new();
    let mut f1s = Vec::new();
    for n in &names {
        let t = *tp.get(n).unwrap_or(&0) as f64;
        let p = *predicted_count.get(n).unwrap_or(&0) as f64;
        let a = *actual_count.get(n).unwrap_or(&0) as f64;
        let precision = if p > 0.0 { t / p } else { 0.0 };
        let recall = if a > 0.0 { t / a } else { 0.0 };
        let f1 = if precision + recall > 0.0 {
            2.0 * precision * recall / (precision + recall)
        } else {
            0.0
        };
        f1s.push(f1);
        classes.push(ClassReport {
            name: n.to_string(),
            precision,
            recall,
            support: a as usize,
        });
    }

    // The curve the design actually needs: a rule that only fires above a
    // confidence floor trades coverage for accuracy, and this says at what
    // rate. Everything below the floor falls through to the next rule, which
    // is a routing decision, not an error.
    let mut coverage = Vec::new();
    for floor in [0.0f32, 0.02, 0.05, 0.10, 0.15, 0.20] {
        let kept: Vec<&(f32, bool)> = decisions.iter().filter(|(m, _)| *m >= floor).collect();
        if kept.is_empty() {
            coverage.push((floor, 0.0, 0.0));
            continue;
        }
        let acc = kept.iter().filter(|(_, ok)| *ok).count() as f64 / kept.len() as f64;
        coverage.push((floor, kept.len() as f64 / decisions.len() as f64, acc));
    }

    Report {
        accuracy,
        macro_f1: f1s.iter().sum::<f64>() / f1s.len() as f64,
        classes,
        coverage,
    }
}

fn print_report(title: &str, r: &Report, show_classes: bool) {
    println!(
        "    {title}: accuracy {:.1}%  macro-F1 {:.3}",
        r.accuracy * 100.0,
        r.macro_f1
    );
    if show_classes {
        for c in &r.classes {
            println!(
                "      {:<12} precision {:>5.1}%  recall {:>5.1}%  (n={})",
                c.name,
                c.precision * 100.0,
                c.recall * 100.0,
                c.support
            );
        }
    }
    let curve: Vec<String> = r
        .coverage
        .iter()
        .map(|(f, cov, acc)| format!("{f:.2}→{:.0}%/{:.1}%", cov * 100.0, acc * 100.0))
        .collect();
    println!("      floor→coverage/accuracy: {}", curve.join("  "));
}

fn main() {
    let no_robots = load("no_robots.json");
    let gsm8k = load("gsm8k.json");
    println!(
        "no_robots: {} prompts, gsm8k: {} prompts\n",
        no_robots.len(),
        gsm8k.len()
    );

    // 1. Coding vs everything else.
    let binary: Vec<Labelled> = no_robots
        .iter()
        .map(|r| Labelled {
            prompt: r.prompt.clone(),
            category: if r.category == "Coding" {
                "Coding"
            } else {
                "Other"
            }
            .to_string(),
        })
        .collect();

    // 3. Reasoning vs lookup: maths word problems against factual questions.
    //    Both are short, both are questions; the only difference is whether
    //    answering needs several steps of work.
    let mut difficulty: Vec<Labelled> = gsm8k
        .iter()
        .map(|r| Labelled {
            prompt: r.prompt.clone(),
            category: "NeedsReasoning".to_string(),
        })
        .collect();
    difficulty.extend(
        no_robots
            .iter()
            .filter(|r| r.category == "Open QA")
            .take(1000)
            .map(|r| Labelled {
                prompt: r.prompt.clone(),
                category: "Lookup".to_string(),
            }),
    );

    for (name, repo) in MODELS {
        print!("{name} ... ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let start = Instant::now();
        let model = match StaticModel::from_pretrained(repo, None, None, None) {
            Ok(m) => m,
            Err(e) => {
                println!("FAILED: {e}");
                continue;
            }
        };
        println!("loaded in {:?}", start.elapsed());

        for cap in CAPS {
            println!("  max_length {cap}:");
            print_report(
                "coding vs other",
                &evaluate(&model, &binary, *cap, 0.7),
                true,
            );
            print_report(
                "10-way category",
                &evaluate(&model, &no_robots, *cap, 0.7),
                false,
            );
            print_report(
                "reasoning vs lookup",
                &evaluate(&model, &difficulty, *cap, 0.7),
                true,
            );
        }
        println!();
    }
}
