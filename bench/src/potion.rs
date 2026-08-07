//! Evaluate Model2Vec static embedding models as a routing classifier.
//!
//! Two questions, and throughput only answers the first:
//!
//! 1. **What does it cost on the request path?** Anything here runs before the
//!    upstream call, inside the budget that currently measures as zero overhead
//!    against a real vLLM (83.2ms TTFT proxied vs 83.8ms direct). A classifier
//!    costing 1ms is 1.2% of that; one costing 20ms is not viable at all.
//!
//! 2. **Can it actually separate our traffic?** A fast model that cannot tell a
//!    coding question from a chat message is worthless for routing, and would
//!    be worse than worthless in production — a misrouted hard question comes
//!    back as a bad answer that looks like the gateway's fault. So this also
//!    measures classification accuracy against a labelled prompt set, using
//!    exactly the scheme the real thing would use: class centroids built from
//!    example prompts, cosine similarity, nearest centroid wins.
//!
//! Run with `--release`; a debug build is >10x slower and would say nothing
//! useful about the hot path.
//!
//! ```text
//! cargo run -p bench --release --bin potion
//! ```

use model2vec_rs::model::StaticModel;
use std::time::{Duration, Instant};

/// Every model worth considering, smallest first.
const MODELS: &[(&str, &str)] = &[
    ("potion-base-2M", "minishlab/potion-base-2M"),
    ("potion-base-4M", "minishlab/potion-base-4M"),
    ("potion-base-8M", "minishlab/potion-base-8M"),
    ("potion-code-16M", "minishlab/potion-code-16M"),
    ("potion-retrieval-32M", "minishlab/potion-retrieval-32M"),
];

/// Prompt sizes that bracket what the proxy actually sees: a one-line chat
/// turn, a normal question, a pasted function, and a large pasted file.
fn prompt_sizes() -> Vec<(&'static str, String)> {
    let unit = "Explain what this function does and suggest an improvement. ";
    vec![
        ("tiny ~40B", "What is the capital of France?".to_string()),
        ("small ~400B", unit.repeat(7)),
        ("medium ~4KB", unit.repeat(70)),
        ("large ~16KB", unit.repeat(280)),
        ("huge ~64KB", unit.repeat(1120)),
    ]
}

// ---------------------------------------------------------------------------
// Labelled prompts
// ---------------------------------------------------------------------------

/// Three classes, chosen to match the actual routing question: send code work
/// to a coding model, hard analytical work to an expensive model, and small
/// talk to whatever is cheapest.
///
/// Deliberately *not* cherry-picked to be easy — several `chat` prompts use
/// technical vocabulary, and several `reasoning` prompts are short, because
/// that is where a bag-of-embeddings classifier is weakest and where the
/// production failure would come from.
const CODING: &[&str] = &[
    "Write a Python function that merges two sorted lists.",
    "Why does this Rust code fail the borrow checker? fn main() { let v = vec![1]; let r = &v; drop(v); println!(\"{:?}\", r); }",
    "Refactor this class to use dependency injection.",
    "Convert this SQL query into an ORM call in Django.",
    "My unit test throws NullPointerException on line 42, here is the stack trace.",
    "Add error handling to this async fetch call in TypeScript.",
    "Explain what this regex does: ^(?:[a-z0-9!#$%&'*+/=?^_`{|}~-]+)@example\\.com$",
    "Write a Dockerfile for a Go service that listens on port 8080.",
    "How do I fix 'cannot borrow as mutable more than once' in this loop?",
    "Generate a SQL migration that adds a nullable timestamp column.",
    "Implement binary search in C without recursion.",
    "What is the time complexity of this nested loop and how do I improve it?",
    "Set up a GitHub Actions workflow that runs cargo test on push.",
    "Parse this JSON into a struct with serde and handle the optional fields.",
    "Why is my Kubernetes pod stuck in CrashLoopBackOff?",
    "Write a shell script that rotates log files older than 7 days.",
    "Translate this Java interface into idiomatic Rust traits.",
    "Debug this segfault in my C++ pointer arithmetic.",
    "Add pagination to this REST endpoint.",
    "Write a property-based test for this parser.",
];

const REASONING: &[&str] = &[
    "Prove that the square root of two is irrational.",
    "Three people check into a hotel and pay 30 dollars. The clerk realises the room is 25 and sends back 5 via a bellhop who keeps 2. Where is the missing dollar?",
    "Design a distributed rate limiter that stays correct under network partition, and justify the consistency model you chose.",
    "A company's revenue grew 40% while margin fell 12 points. Under what conditions is that a good outcome?",
    "Given the trolley problem, construct the strongest argument against the utilitarian answer.",
    "If all bloops are razzies and all razzies are lazzies, are all bloops definitely lazzies? Explain your reasoning step by step.",
    "Derive the closed form for the sum of the first n cubes.",
    "Compare eventual and strong consistency for a payment ledger, and say which you would choose and why.",
    "What is the expected number of coin flips to get two heads in a row?",
    "Critique this experimental design: we A/B tested for three days and saw a 2% lift with p=0.06.",
    "Explain the halting problem and why it implies limits on static analysis.",
    "Two trains leave stations 300km apart travelling toward each other at 60 and 90 km/h. A bird flies between them at 120 km/h until they meet. How far does the bird fly?",
    "Argue both sides of whether a central bank should target nominal GDP instead of inflation.",
    "Why does the Monty Hall answer change if the host opens a door at random?",
    "Formulate the tradeoff between recall and precision for a fraud model where a false positive costs 50 dollars and a false negative costs 4000.",
    "Is it rational to one-box in Newcomb's problem? Defend your position.",
    "Estimate how many piano tuners work in Chicago and show your assumptions.",
    "Explain why Simpson's paradox arises and give a real example.",
    "What are the second-order effects of removing rent control in a supply-constrained city?",
    "Reconcile these two studies with opposite conclusions about remote work productivity.",
];

const CHAT: &[&str] = &[
    "Hi there!",
    "What is the capital of France?",
    "Thanks, that was helpful.",
    "Can you summarise this article in two sentences?",
    "What time zone is Amsterdam in?",
    "Tell me a joke about computers.",
    "Translate 'good morning' into Japanese.",
    "What's the weather like usually in Lisbon in May?",
    "Give me three ideas for a team offsite.",
    "Who wrote Pride and Prejudice?",
    "Rewrite this paragraph to sound more formal.",
    "What does the acronym API stand for?",
    "Recommend a book about the history of the internet.",
    "How do you say thank you in Portuguese?",
    "Make a packing list for a weekend hiking trip.",
    "What's a good name for a golden retriever puppy?",
    "Summarise the plot of Hamlet briefly.",
    "Convert 72 fahrenheit to celsius.",
    "What are the opening hours of most Dutch supermarkets?",
    "Write a short thank-you note to a colleague.",
];

fn classes() -> Vec<(&'static str, &'static [&'static str])> {
    vec![("coding", CODING), ("reasoning", REASONING), ("chat", CHAT)]
}

// ---------------------------------------------------------------------------
// Vector helpers
// ---------------------------------------------------------------------------

fn normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn centroid(vectors: &[Vec<f32>]) -> Vec<f32> {
    let dim = vectors[0].len();
    let mut sum = vec![0.0f32; dim];
    for v in vectors {
        for (s, x) in sum.iter_mut().zip(v) {
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

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

// ---------------------------------------------------------------------------

struct Latency {
    label: &'static str,
    p50: Duration,
    p99: Duration,
    per_sec: f64,
}

fn measure_latency_at(
    model: &StaticModel,
    label: &'static str,
    text: &str,
    max_length: Option<usize>,
) -> Latency {
    let one = [text.to_string()];
    for _ in 0..20 {
        let _ = model.encode_with_args(&one, max_length, 1);
    }
    let iterations = 1000;
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let v = model.encode_with_args(&one, max_length, 1);
        samples.push(start.elapsed());
        std::hint::black_box(v);
    }
    samples.sort();
    let total: Duration = samples.iter().sum();
    Latency {
        label,
        p50: percentile(&samples, 0.50),
        p99: percentile(&samples, 0.99),
        per_sec: iterations as f64 / total.as_secs_f64(),
    }
}

fn measure_latency(model: &StaticModel, label: &'static str, text: &str) -> Latency {
    // Warm up: the first call touches pages of the embedding table that later
    // calls find resident, and reporting that as p50 would overstate the cost
    // of every request after the first.
    for _ in 0..20 {
        let _ = model.encode_single(text);
    }
    let iterations = if text.len() > 8192 { 200 } else { 2000 };
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let v = model.encode_single(text);
        samples.push(start.elapsed());
        // Keep the optimiser from deleting the work entirely.
        std::hint::black_box(v);
    }
    samples.sort();
    let total: Duration = samples.iter().sum();
    Latency {
        label,
        p50: percentile(&samples, 0.50),
        p99: percentile(&samples, 0.99),
        per_sec: iterations as f64 / total.as_secs_f64(),
    }
}

struct Quality {
    accuracy: f64,
    mean_margin: f32,
    worst_margin: f32,
    confusions: Vec<(String, String, String)>,
}

/// Leave-one-out: every prompt is classified against centroids built from the
/// *other* prompts of its class. With twenty examples per class this is a far
/// more honest estimate than a single train/test split, and it is what an
/// operator's first twenty examples would actually behave like.
fn measure_quality(model: &StaticModel) -> Quality {
    measure_quality_at(model, None)
}

/// `max_length` is the token cap the encoder applies. It matters twice over:
/// it bounds what a long prompt costs (the whole reason latency plateaus
/// above ~512 tokens), and it decides how much of the prompt the routing
/// decision is allowed to see. Cheaper and more myopic are the same dial.
fn measure_quality_at(model: &StaticModel, max_length: Option<usize>) -> Quality {
    let classes = classes();
    let embedded: Vec<(&str, Vec<Vec<f32>>)> = classes
        .iter()
        .map(|(name, prompts)| {
            let owned: Vec<String> = prompts.iter().map(|s| s.to_string()).collect();
            let mut vectors = model.encode_with_args(&owned, max_length, 1024);
            for v in vectors.iter_mut() {
                normalise(v);
            }
            (*name, vectors)
        })
        .collect();

    let mut correct = 0usize;
    let mut total = 0usize;
    let mut margins: Vec<f32> = Vec::new();
    let mut confusions = Vec::new();

    for (held_class_idx, (held_name, held_vectors)) in embedded.iter().enumerate() {
        for (held_idx, held) in held_vectors.iter().enumerate() {
            // Centroids excluding the held-out prompt.
            let mut scored: Vec<(&str, f32)> = embedded
                .iter()
                .enumerate()
                .map(|(class_idx, (name, vectors))| {
                    let subset: Vec<Vec<f32>> = vectors
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !(class_idx == held_class_idx && *i == held_idx))
                        .map(|(_, v)| v.clone())
                        .collect();
                    (*name, cosine(held, &centroid(&subset)))
                })
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

            total += 1;
            if scored[0].0 == *held_name {
                correct += 1;
            } else {
                confusions.push((
                    held_name.to_string(),
                    scored[0].0.to_string(),
                    classes[held_class_idx].1[held_idx]
                        .chars()
                        .take(52)
                        .collect(),
                ));
            }
            // Margin between the winner and the runner-up. This is what a
            // confidence floor would be set against: a classifier that is
            // right but barely is one that will be wrong on traffic slightly
            // unlike the examples.
            margins.push(scored[0].1 - scored[1].1);
        }
    }

    let mean_margin = margins.iter().sum::<f32>() / margins.len() as f32;
    let worst_margin = margins.iter().copied().fold(f32::INFINITY, f32::min);
    Quality {
        accuracy: correct as f64 / total as f64,
        mean_margin,
        worst_margin,
        confusions,
    }
}

fn main() {
    println!("Model2Vec routing-classifier evaluation");
    println!(
        "cores: {}\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );

    let sizes = prompt_sizes();
    let mut summary: Vec<(String, Duration, f64, f64, f32)> = Vec::new();

    for (name, repo) in MODELS {
        print!("loading {name} ... ");
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
        let load = start.elapsed();
        let dim = model.encode_single("probe").len();
        println!("{:?}, {dim} dimensions", load);

        println!("  latency (single prompt, release build):");
        let mut tiny_p50 = Duration::ZERO;
        for (label, text) in &sizes {
            let l = measure_latency(&model, label, text);
            if l.label == "tiny ~40B" {
                tiny_p50 = l.p50;
            }
            println!(
                "    {:<14} p50 {:>9.1?}  p99 {:>9.1?}  {:>10.0}/s",
                l.label, l.p50, l.p99, l.per_sec
            );
        }

        let q = measure_quality(&model);
        println!(
            "  classification: accuracy {:.1}%  mean margin {:.4}  worst margin {:.4}",
            q.accuracy * 100.0,
            q.mean_margin,
            q.worst_margin
        );
        for (actual, predicted, prompt) in q.confusions.iter().take(6) {
            println!("    MISS {actual} -> {predicted}: {prompt}...");
        }
        println!();

        summary.push((
            name.to_string(),
            tiny_p50,
            q.accuracy,
            q.mean_margin as f64,
            q.worst_margin,
        ));
    }

    // The token cap is the one tuning dial that trades cost against how much
    // of the prompt the decision sees. Swept on the two models the results
    // above make worth keeping.
    println!("token-cap sweep (a long prompt, so the cap actually binds):");
    let long = "Explain what this function does and suggest an improvement. ".repeat(280);
    for (name, repo) in MODELS
        .iter()
        .filter(|(n, _)| {
            *n == "potion-base-8M" || *n == "potion-code-16M" || *n == "potion-retrieval-32M"
        })
    {
        let Ok(model) = StaticModel::from_pretrained(repo, None, None, None) else {
            continue;
        };
        println!("  {name}:");
        println!(
            "    {:>10} {:>10} {:>10} {:>12}",
            "max_length", "p50", "accuracy", "mean margin"
        );
        for cap in [32usize, 64, 128, 256, 512] {
            let l = measure_latency_at(&model, "long", &long, Some(cap));
            let q = measure_quality_at(&model, Some(cap));
            println!(
                "    {:>10} {:>10.1?} {:>9.1}% {:>12.4}",
                cap,
                l.p50,
                q.accuracy * 100.0,
                q.mean_margin
            );
        }
    }
    println!();

    println!("summary (tiny-prompt p50 is what a routing decision costs):");
    println!(
        "  {:<22} {:>10} {:>10} {:>12} {:>13}",
        "model", "p50", "accuracy", "mean margin", "worst margin"
    );
    for (name, p50, acc, mean, worst) in &summary {
        println!(
            "  {:<22} {:>10.1?} {:>9.1}% {:>12.4} {:>13.4}",
            name,
            p50,
            acc * 100.0,
            mean,
            worst
        );
    }
}
