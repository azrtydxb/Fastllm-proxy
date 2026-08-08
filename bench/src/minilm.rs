//! Does a real transformer earn its latency on the splits a static model fails?
//!
//! `potion-code-16M` classifies in ~140µs and separates coding, maths, chat,
//! generation and factual-QA at 90-98% precision. It cannot separate
//! *architecture* from *coding*: centroid similarity 0.511, and architecture
//! precision peaks at 72% while costing coding half its recall.
//!
//! The reason is structural. Static embeddings are a bag of token vectors with
//! no word order, and the difference between "design a rate limiter" and "debug
//! my rate limiter" is the verb, not the subject. Both bags are nearly
//! identical. A contextual model sees the difference or nothing will.
//!
//! So: same data, same protocol, MiniLM instead. If it does not clear the
//! static model by a wide margin, a two-tier cascade is not worth building.
//!
//! ```text
//! cargo run -p bench --release --bin minilm
//! ```

use fastembed::{
    EmbeddingModel, InitOptions, InitOptionsUserDefined, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use std::collections::HashMap;
use std::time::Instant;

const SEEDS: usize = 40;

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

/// Truncate to roughly the same window the static model saw, so the comparison
/// is between models rather than between amounts of text.
fn clip(s: &str) -> String {
    s.chars().take(600).collect()
}

fn embed(model: &mut TextEmbedding, rows: &[Labelled]) -> Vec<Vec<f32>> {
    let texts: Vec<String> = rows.iter().map(|r| clip(&r.prompt)).collect();
    let mut v = model.embed(texts, Some(256)).expect("embed");
    for x in v.iter_mut() {
        normalise(x);
    }
    v
}

fn score(sets: &[(&str, &Vec<Vec<f32>>)], floor: f32) -> (f64, f64, Vec<(String, f64, f64)>) {
    let centroids: Vec<(&str, Vec<f32>)> = sets
        .iter()
        .map(|(n, v)| (*n, mean(&v[..SEEDS].iter().collect::<Vec<_>>())))
        .collect();
    let mut tp: HashMap<&str, usize> = HashMap::new();
    let mut pred: HashMap<&str, usize> = HashMap::new();
    let mut act: HashMap<&str, usize> = HashMap::new();
    let (mut kept, mut total) = (0usize, 0usize);
    for (name, vectors) in sets {
        for v in &vectors[SEEDS..] {
            total += 1;
            let mut scored: Vec<(&str, f32)> =
                centroids.iter().map(|(n, c)| (*n, cosine(v, c))).collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            if scored[0].1 - scored[1].1 < floor {
                continue;
            }
            kept += 1;
            *pred.entry(scored[0].0).or_default() += 1;
            *act.entry(name).or_default() += 1;
            if scored[0].0 == *name {
                *tp.entry(name).or_default() += 1;
            }
        }
    }
    let correct: usize = tp.values().sum();
    let per_class = sets
        .iter()
        .map(|(n, _)| {
            let t = *tp.get(n).unwrap_or(&0) as f64;
            let p = *pred.get(n).unwrap_or(&0) as f64;
            let a = *act.get(n).unwrap_or(&0) as f64;
            (
                n.to_string(),
                if p > 0.0 { t / p } else { 0.0 },
                if a > 0.0 { t / a } else { 0.0 },
            )
        })
        .collect();
    (
        kept as f64 / total as f64,
        correct as f64 / kept.max(1) as f64,
        per_class,
    )
}

/// Load a model from a directory of ONNX + tokeniser files.
///
/// Exists so a *quantised* model can be measured on the same data as the
/// stock one: the accuracy this benchmark reports is the whole reason the
/// refined tier is worth its cost, so swapping the weights for smaller ones
/// has to be gated on it rather than on the latency alone.
fn from_dir(dir: &str) -> anyhow::Result<TextEmbedding> {
    let read = |name: &str| std::fs::read(std::path::Path::new(dir).join(name));
    let files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    let model = UserDefinedEmbeddingModel::new(read("model.onnx")?, files);
    Ok(TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::new().with_max_length(256),
    )?)
}

fn main() {
    let arch = load("se_architecture.json");
    let code = load("se_codereview.json");

    // Any directories given on the command line are measured too, labelled by
    // path — that is how a candidate replacement model gets compared against
    // the incumbent on identical data in one run.
    let extra: Vec<String> = std::env::args().skip(1).collect();
    let mut models: Vec<(String, Option<EmbeddingModel>)> = vec![
        (
            "all-MiniLM-L6-v2".into(),
            Some(EmbeddingModel::AllMiniLML6V2),
        ),
        (
            "bge-small-en-v1.5".into(),
            Some(EmbeddingModel::BGESmallENV15),
        ),
    ];
    models.extend(extra.into_iter().map(|d| (d, None)));

    for (label, which) in models {
        println!("\n================ {label} ================");
        let loaded = match which {
            Some(w) => {
                TextEmbedding::try_new(InitOptions::new(w).with_show_download_progress(false))
                    .map_err(anyhow::Error::from)
            }
            None => from_dir(&label),
        };
        let mut model = match loaded {
            Ok(m) => m,
            Err(e) => {
                println!("could not load: {e}");
                continue;
            }
        };

        // Per-prompt latency, which is the number that decides whether this can
        // sit on the request path at all.
        let one = vec![clip(&arch[0].prompt)];
        for _ in 0..5 {
            let _ = model.embed(one.clone(), Some(256));
        }
        let mut samples = Vec::new();
        for _ in 0..100 {
            let start = Instant::now();
            let v = model.embed(one.clone(), Some(256)).expect("embed");
            samples.push(start.elapsed());
            std::hint::black_box(v);
        }
        samples.sort();
        println!(
            "  single-prompt latency: p50 {:?}  p99 {:?}",
            samples[50], samples[98]
        );

        let arch_v = embed(&mut model, &arch);
        let code_v = embed(&mut model, &code);
        let sets: Vec<(&str, &Vec<Vec<f32>>)> =
            vec![("architecture", &arch_v), ("code-review", &code_v)];

        let ca = mean(&arch_v[..SEEDS].iter().collect::<Vec<_>>());
        let cc = mean(&code_v[..SEEDS].iter().collect::<Vec<_>>());
        println!(
            "  centroid similarity architecture <-> code-review: {:.3}  \
             (potion-code-16M: 0.621)",
            cosine(&ca, &cc)
        );

        for floor in [0.0f32, 0.02, 0.05, 0.10] {
            let (coverage, accuracy, per_class) = score(&sets, floor);
            println!(
                "  floor {floor:.2}: coverage {:.0}%  accuracy {:.1}%",
                coverage * 100.0,
                accuracy * 100.0
            );
            if floor == 0.05 {
                for (n, p, r) in per_class {
                    println!(
                        "      {n:<14} precision {:>5.1}%  recall {:>5.1}%",
                        p * 100.0,
                        r * 100.0
                    );
                }
            }
        }
    }
}
