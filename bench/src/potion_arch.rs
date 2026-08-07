//! Can a static embedding tell *architecture* from *coding*?
//!
//! The routing question behind this: "design me a service topology" and "why
//! does this function segfault" both want a strong model, but not the same one.
//! Sending an architecture question to a code-specialist model wastes the
//! reasoning it needed; sending a segfault to a general reasoning model wastes
//! the code training it needed.
//!
//! Testing it with invented seeds would prove nothing — the previous probe
//! showed hand-written examples look more separable than they are. So this uses
//! a natural experiment instead: two StackExchange communities that separated
//! the traffic themselves.
//!
//!   softwareengineering.SE — design, patterns, architecture, "should I"
//!   codereview.SE          — concrete code, "here is my implementation"
//!   devops.SE              — operational, a third neighbour to check bleed
//!
//! Both are written by programmers about programs, in the same vocabulary,
//! which is precisely what makes this hard and worth measuring rather than
//! assuming.
//!
//! ```text
//! python3 bench/fetch-prompts.py
//! cargo run -p bench --release --bin potion-arch
//! ```

use model2vec_rs::model::StaticModel;
use std::collections::HashMap;

const CAP: usize = 128;
const SEEDS: usize = 40;

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

struct Set {
    name: &'static str,
    vectors: Vec<Vec<f32>>,
}

/// Seed centroids from the first `SEEDS` of each class, evaluate on the rest.
fn evaluate(sets: &[Set], floor: f32) -> (f64, f64, Vec<(String, f64, f64, usize)>) {
    let centroids: Vec<(&str, Vec<f32>)> = sets
        .iter()
        .map(|s| (s.name, mean(&s.vectors[..SEEDS].iter().collect::<Vec<_>>())))
        .collect();

    let mut tp: HashMap<&str, usize> = HashMap::new();
    let mut pred: HashMap<&str, usize> = HashMap::new();
    let mut act: HashMap<&str, usize> = HashMap::new();
    let (mut kept, mut total) = (0usize, 0usize);

    for s in sets {
        for v in &s.vectors[SEEDS..] {
            total += 1;
            let mut scored: Vec<(&str, f32)> =
                centroids.iter().map(|(n, c)| (*n, cosine(v, c))).collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            if scored[0].1 - scored[1].1 < floor {
                continue;
            }
            kept += 1;
            *pred.entry(scored[0].0).or_default() += 1;
            *act.entry(s.name).or_default() += 1;
            if scored[0].0 == s.name {
                *tp.entry(s.name).or_default() += 1;
            }
        }
    }

    let correct: usize = tp.values().sum();
    let per_class = sets
        .iter()
        .map(|s| {
            let t = *tp.get(s.name).unwrap_or(&0) as f64;
            let p = *pred.get(s.name).unwrap_or(&0) as f64;
            let a = *act.get(s.name).unwrap_or(&0) as f64;
            (
                s.name.to_string(),
                if p > 0.0 { t / p } else { 0.0 },
                if a > 0.0 { t / a } else { 0.0 },
                a as usize,
            )
        })
        .collect();

    (
        kept as f64 / total as f64,
        correct as f64 / kept.max(1) as f64,
        per_class,
    )
}

fn report(title: &str, sets: &[Set]) {
    println!("\n--- {title} ---");
    // Centroid similarity first: it is the cheap check that says whether the
    // split is possible at all, before any per-class number is worth reading.
    let centroids: Vec<(&str, Vec<f32>)> = sets
        .iter()
        .map(|s| (s.name, mean(&s.vectors[..SEEDS].iter().collect::<Vec<_>>())))
        .collect();
    for (i, (na, a)) in centroids.iter().enumerate() {
        for (nb, b) in centroids.iter().skip(i + 1) {
            println!("  centroid similarity {na} <-> {nb}: {:.3}", cosine(a, b));
        }
    }
    for floor in [0.0f32, 0.05, 0.10, 0.15, 0.20] {
        let (coverage, accuracy, per_class) = evaluate(sets, floor);
        println!(
            "  floor {floor:.2}: coverage {:.0}%  accuracy {:.1}%",
            coverage * 100.0,
            accuracy * 100.0
        );
        if floor >= 0.05 {
            for (name, precision, recall, n) in per_class {
                println!(
                    "      {name:<20} precision {:>5.1}%  recall {:>5.1}%  (n={n})",
                    precision * 100.0,
                    recall * 100.0
                );
            }
        }
    }
}

fn embed(model: &StaticModel, rows: &[Labelled]) -> Vec<Vec<f32>> {
    let texts: Vec<String> = rows.iter().map(|r| r.prompt.clone()).collect();
    let mut v = model.encode_with_args(&texts, Some(CAP), 1024);
    for x in v.iter_mut() {
        normalise(x);
    }
    v
}

fn main() {
    let arch = load("se_architecture.json");
    let code = load("se_codereview.json");
    let devops = load("se_devops.json");
    let no_robots = load("no_robots.json");
    let gsm8k = load("gsm8k.json");

    for model_id in [
        "minishlab/potion-code-16M",
        "minishlab/potion-base-8M",
        "minishlab/potion-retrieval-32M",
    ] {
        println!("\n================ {model_id} ================");
        let model = StaticModel::from_pretrained(model_id, None, None, None).expect("load");

        let arch_v = embed(&model, &arch);
        let code_v = embed(&model, &code);
        let devops_v = embed(&model, &devops);

        // The question as asked: architecture against concrete code, nothing
        // else in the way.
        report(
            "architecture vs code-review (the question)",
            &[
                Set {
                    name: "architecture",
                    vectors: arch_v.clone(),
                },
                Set {
                    name: "code-review",
                    vectors: code_v.clone(),
                },
            ],
        );

        // With a third technical neighbour, because a two-way result flatters
        // itself: any vector must land somewhere, and with only two options a
        // coin flip scores 50%.
        report(
            "architecture vs code-review vs devops",
            &[
                Set {
                    name: "architecture",
                    vectors: arch_v.clone(),
                },
                Set {
                    name: "code-review",
                    vectors: code_v.clone(),
                },
                Set {
                    name: "devops",
                    vectors: devops_v,
                },
            ],
        );

        // And in the company it would actually keep: the five classes that
        // survived the previous probe. If architecture only separates when
        // nothing else is present, it is not a class, it is an artefact.
        let coding: Vec<Labelled> = no_robots
            .iter()
            .filter(|r| r.category == "Coding")
            .cloned()
            .collect();
        let chat: Vec<Labelled> = no_robots
            .iter()
            .filter(|r| r.category == "Chat")
            .cloned()
            .collect();
        let generation: Vec<Labelled> = no_robots
            .iter()
            .filter(|r| r.category == "Generation")
            .take(1200)
            .cloned()
            .collect();
        let factual: Vec<Labelled> = no_robots
            .iter()
            .filter(|r| r.category == "Open QA")
            .cloned()
            .collect();

        report(
            "the full six-class taxonomy",
            &[
                Set {
                    name: "architecture",
                    vectors: arch_v,
                },
                Set {
                    name: "coding",
                    vectors: [embed(&model, &coding), code_v].concat(),
                },
                Set {
                    name: "math",
                    vectors: embed(&model, &gsm8k),
                },
                Set {
                    name: "chat",
                    vectors: embed(&model, &chat),
                },
                Set {
                    name: "generation",
                    vectors: embed(&model, &generation),
                },
                Set {
                    name: "factual-qa",
                    vectors: embed(&model, &factual),
                },
            ],
        );
    }
}
