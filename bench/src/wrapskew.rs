//! Does the request path classify what the control plane trained on?
//!
//! The control plane builds a class centroid from example prompts an operator
//! typed: bare text. The request path embeds the **raw request body** — JSON
//! syntax, role labels, system prompt and all — and only the first 128 tokens
//! of it.
//!
//! Those are two different text distributions, and nearest-centroid
//! classification has no way to know. Everything in `docs/classifier.md` was
//! measured bare-against-bare, so this measures the asymmetry production
//! actually runs: centroids from bare prompts, queries from whole bodies.
//!
//! Four query shapes, each one more like a real request than the last.

use model2vec_rs::model::StaticModel;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Clone)]
struct Row {
    prompt: String,
    category: String,
}

fn load(name: &str) -> Vec<Row> {
    let path = format!("bench/data/{name}");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    serde_json::from_slice(&bytes).expect("dataset is JSON")
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

fn centroid(vs: &[&Vec<f32>]) -> Vec<f32> {
    let mut out = vec![0.0; vs[0].len()];
    for v in vs {
        for (o, x) in out.iter_mut().zip(v.iter()) {
            *o += x;
        }
    }
    normalise(&mut out);
    out
}

fn embed(model: &StaticModel, texts: &[String]) -> Vec<Vec<f32>> {
    // 128 tokens: what `classifier::tier1::MAX_TOKENS` gives the request path.
    let mut v = model.encode_with_args(texts, Some(128), 1024);
    for x in v.iter_mut() {
        normalise(x);
    }
    v
}

const SYSTEM: &str = "You are a helpful assistant. Answer carefully, cite your \
                      reasoning where it matters, and keep responses concise \
                      unless the user asks for detail.";

/// The shapes a prompt can reach the classifier in.
fn shape(kind: &str, prompt: &str) -> String {
    match kind {
        // What every accuracy number in the docs was measured on.
        "bare" => prompt.to_string(),
        // The smallest real request: no system prompt, one turn.
        "body" => serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": prompt}],
        })
        .to_string(),
        // The common real request: a system prompt in front of it.
        "body+system" => serde_json::json!({
            "model": "auto",
            "max_tokens": 512,
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": prompt},
            ],
        })
        .to_string(),
        // Turn four of a conversation that began somewhere else entirely. The
        // prompt being classified is at the end, where the window never reaches.
        "turn 4" => serde_json::json!({
            "model": "auto",
            "max_tokens": 512,
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": "Can you help me plan a dinner party menu for eight people, with two vegetarians?"},
                {"role": "assistant", "content": "Of course. A good structure is three courses with one shared centrepiece."},
                {"role": "user", "content": "Great, and what wine would you pour with that?"},
                {"role": "assistant", "content": "A dry white for the starter and a light red for the main works well."},
                {"role": "user", "content": prompt},
            ],
        })
        .to_string(),
        _ => unreachable!(),
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "models/potion-code-16M".to_string());
    let model = match StaticModel::from_pretrained(&path, None, None, None) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("could not load {path}: {e}");
            eprintln!("pass a model directory as the first argument");
            std::process::exit(1);
        }
    };

    let rows = load("no_robots.json");
    let data: Vec<Row> = rows
        .iter()
        .map(|r| Row {
            prompt: r.prompt.clone(),
            category: if r.category == "Coding" {
                "Coding"
            } else {
                "Other"
            }
            .to_string(),
        })
        .collect();

    // Deterministic split. Centroids come from the training half only, and
    // always from **bare** text — that is what the control plane embeds.
    let split = data.len() / 2;
    let (train, test) = data.split_at(split);

    let train_texts: Vec<String> = train.iter().map(|r| r.prompt.clone()).collect();
    let train_vecs = embed(&model, &train_texts);
    let mut by_class: HashMap<&str, Vec<&Vec<f32>>> = HashMap::new();
    for (row, v) in train.iter().zip(&train_vecs) {
        by_class.entry(row.category.as_str()).or_default().push(v);
    }
    let mut names: Vec<&str> = by_class.keys().copied().collect();
    names.sort();
    let centroids: Vec<(&str, Vec<f32>)> =
        names.iter().map(|n| (*n, centroid(&by_class[n]))).collect();

    println!(
        "centroids from {} bare training prompts; {} test prompts per shape\n",
        train.len(),
        test.len()
    );
    println!(
        "{:<14} {:>9} {:>12} {:>12} {:>14}",
        "query shape", "accuracy", "coding prec", "coding rec", "mean margin"
    );

    for kind in [
        "bare",
        "body",
        "body+system",
        "turn 4",
        // The same three request shapes, put through the extraction the request
        // path now uses. If the fix works these are indistinguishable from
        // "bare", because that is exactly what they reduce to.
        "fixed: body",
        "fixed: body+system",
        "fixed: turn 4",
    ] {
        let texts: Vec<String> = test
            .iter()
            .map(|r| match kind.strip_prefix("fixed: ") {
                Some(inner) => {
                    let body = shape(inner, &r.prompt);
                    fastllm_proxy::classifier::prompt::text_to_classify(body.as_bytes())
                        .unwrap_or_default()
                }
                None => shape(kind, &r.prompt),
            })
            .collect();
        let vecs = embed(&model, &texts);

        let (mut correct, mut tp, mut predicted, mut actual, mut margin_sum) =
            (0usize, 0usize, 0usize, 0usize, 0f32);
        for (row, v) in test.iter().zip(&vecs) {
            let mut scored: Vec<(&str, f32)> =
                centroids.iter().map(|(n, c)| (*n, cosine(v, c))).collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            margin_sum += scored[0].1 - scored.get(1).map(|s| s.1).unwrap_or(0.0);
            if scored[0].0 == row.category {
                correct += 1;
            }
            if scored[0].0 == "Coding" {
                predicted += 1;
                if row.category == "Coding" {
                    tp += 1;
                }
            }
            if row.category == "Coding" {
                actual += 1;
            }
        }
        let pct = |a: usize, b: usize| {
            if b == 0 {
                0.0
            } else {
                100.0 * a as f64 / b as f64
            }
        };
        println!(
            "{kind:<14} {:>8.1}% {:>11.1}% {:>11.1}% {:>14.3}",
            pct(correct, test.len()),
            pct(tp, predicted),
            pct(tp, actual),
            margin_sum / test.len() as f32
        );
    }
}
