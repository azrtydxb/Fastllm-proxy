//! What can this embedding actually tell apart?
//!
//! Choosing routing classes by intuition is how you end up with `Closed QA`
//! and `Extract` — two names for regions of the space that overlap so much the
//! classifier scores 20% precision on them. This asks the model instead, two
//! ways:
//!
//! 1. **Unsupervised.** k-means over ~9.5k real prompts, then print each
//!    cluster's nearest-centroid examples and the human labels that fell into
//!    it. This is the structure the embedding actually has, with nobody's
//!    taxonomy imposed on it.
//!
//! 2. **Supervised, exactly as the product would work.** A candidate ten-class
//!    routing taxonomy, each class defined by a dozen seed prompts — the same
//!    input an operator would give. Then the two diagnostics that matter:
//!    the **centroid similarity matrix** (two classes whose centroids sit at
//!    cosine 0.95 are not distinguishable, and no threshold will save them),
//!    and leave-one-out precision/recall per class.
//!
//! The second is a prototype of the `POST /admin/prompt-classes/evaluate`
//! endpoint in the classifier-routing spec: the point is that an operator gets
//! told which of *their* classes work, rather than being told how many they may
//! have.
//!
//! ```text
//! python3 bench/fetch-prompts.py
//! cargo run -p bench --release --bin potion-classes
//! ```

use model2vec_rs::model::StaticModel;
use std::collections::HashMap;

const MODEL: &str = "minishlab/potion-code-16M";
const CAP: usize = 128;
const K: usize = 10;

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

/// Seeded xorshift. Deterministic on purpose: a clustering that comes out
/// differently each run cannot be discussed, let alone compared against the
/// previous run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Spherical k-means: vectors are unit-length, so cosine similarity is the
/// distance and the mean-then-renormalise step is the correct update.
fn kmeans(vectors: &[Vec<f32>], k: usize, iterations: usize) -> (Vec<Vec<f32>>, Vec<usize>) {
    let mut rng = Rng(0x0C1A_55E5_u64 | 1);
    // k-means++ seeding, which matters here: uniform seeding on text
    // embeddings reliably lands several centroids inside the one dense blob of
    // generic prose and leaves the interesting sparse regions unclaimed.
    let mut centroids: Vec<Vec<f32>> = vec![vectors[rng.below(vectors.len())].clone()];
    while centroids.len() < k {
        let mut best_d: Vec<f32> = vectors
            .iter()
            .map(|v| {
                centroids
                    .iter()
                    .map(|c| 1.0 - cosine(v, c))
                    .fold(f32::INFINITY, f32::min)
            })
            .collect();
        // Sample proportionally to squared distance.
        let total: f32 = best_d.iter().map(|d| d * d).sum();
        let mut target = (rng.next() as f64 / u64::MAX as f64) as f32 * total;
        let mut chosen = vectors.len() - 1;
        for (i, d) in best_d.iter_mut().enumerate() {
            target -= *d * *d;
            if target <= 0.0 {
                chosen = i;
                break;
            }
        }
        centroids.push(vectors[chosen].clone());
    }

    let mut assignment = vec![0usize; vectors.len()];
    for _ in 0..iterations {
        let mut changed = false;
        for (i, v) in vectors.iter().enumerate() {
            let best = centroids
                .iter()
                .enumerate()
                .max_by(|a, b| cosine(v, a.1).partial_cmp(&cosine(v, b.1)).unwrap())
                .map(|(idx, _)| idx)
                .unwrap();
            if assignment[i] != best {
                assignment[i] = best;
                changed = true;
            }
        }
        for (c, centroid) in centroids.iter_mut().enumerate() {
            let members: Vec<&Vec<f32>> = vectors
                .iter()
                .zip(&assignment)
                .filter(|(_, a)| **a == c)
                .map(|(v, _)| v)
                .collect();
            if !members.is_empty() {
                *centroid = mean(&members);
            }
        }
        if !changed {
            break;
        }
    }
    (centroids, assignment)
}

// ---------------------------------------------------------------------------
// A candidate taxonomy: ten classes an LLM gateway's customer would plausibly
// want, seeded the way an operator would seed them.
// ---------------------------------------------------------------------------

const TAXONOMY: &[(&str, &[&str])] = &[
    (
        "code-write",
        &[
            "Write a Python function that merges two sorted lists.",
            "Implement a binary search tree in Rust with insert and delete.",
            "Create a REST endpoint in Express that accepts a JSON payload.",
            "Write a SQL query that returns the top 10 customers by revenue.",
            "Generate a Dockerfile for a Go service listening on port 8080.",
            "Add pagination to this Django list view.",
            "Write a bash script that archives logs older than 30 days.",
            "Build a React component that renders a sortable table.",
            "Implement rate limiting middleware for a Node server.",
            "Write a unit test for this parser using pytest.",
            "Create a Kubernetes deployment manifest with two replicas.",
            "Write a regex that validates an ISO 8601 timestamp.",
        ],
    ),
    (
        "code-debug",
        &[
            "Why does this code throw NullPointerException on line 42?",
            "My Rust program fails to compile with 'cannot borrow as mutable twice'.",
            "This SQL query returns duplicate rows, what is wrong with the join?",
            "Segfault in my C++ code when I dereference this pointer, here is the trace.",
            "My Kubernetes pod is stuck in CrashLoopBackOff, here are the logs.",
            "The test passes locally but fails in CI with a timeout.",
            "Why is this async function returning a pending promise?",
            "My Docker build fails at the COPY step with 'file not found'.",
            "This loop never terminates, can you spot the bug?",
            "Getting 'connection refused' from my service, how do I diagnose it?",
            "Why does my Python import fail with circular import error?",
            "This memory leak grows 100MB an hour, where should I look?",
        ],
    ),
    (
        "math-reasoning",
        &[
            "If a train travels 120km in 90 minutes, what is its average speed?",
            "Prove that the sum of two even numbers is even.",
            "What is the expected value of rolling two dice and taking the maximum?",
            "Solve for x: 3x squared minus 12x plus 9 equals zero.",
            "A shop sells apples at 3 for 2 euros. How much for 17 apples?",
            "Derive the formula for the sum of an arithmetic series.",
            "What is the probability of drawing two aces from a shuffled deck?",
            "Calculate the compound interest on 5000 at 4% over 7 years.",
            "How many ways can 8 people be seated around a round table?",
            "If f(x) = 2x + 3, what is the inverse function?",
            "A tank fills in 6 hours and drains in 10. How long to fill both open?",
            "What is the area under the curve y = x squared from 0 to 3?",
        ],
    ),
    (
        "analysis-advice",
        &[
            "Should we migrate our monolith to microservices given a team of six?",
            "Compare the tradeoffs of renting versus buying office space.",
            "What are the second-order effects of raising our prices by 20%?",
            "Evaluate whether we should build this in-house or buy a vendor product.",
            "Our churn rose from 3% to 5% this quarter. What should we investigate?",
            "Argue both sides of adopting a four-day work week.",
            "How should we prioritise these five features given limited engineers?",
            "What risks should we consider before entering the German market?",
            "Critique this go-to-market plan for a developer tool.",
            "Is it better to hire a senior engineer or two juniors right now?",
            "Assess whether this acquisition makes strategic sense.",
            "What metrics should we track to know if this redesign worked?",
        ],
    ),
    (
        "summarise",
        &[
            "Summarise this article in three sentences.",
            "Give me a one-paragraph summary of the meeting notes below.",
            "Condense this report into five bullet points.",
            "What are the key takeaways from this transcript?",
            "TLDR this thread for me.",
            "Summarise the main argument of this essay.",
            "Give me the highlights of this earnings call.",
            "Shorten this into an executive summary.",
            "What is the gist of this legal document?",
            "Boil this down to the three most important findings.",
            "Provide an abstract for this paper.",
            "Summarise the plot of this chapter briefly.",
        ],
    ),
    (
        "rewrite-edit",
        &[
            "Rewrite this paragraph to sound more formal.",
            "Fix the grammar and spelling in this text.",
            "Make this email shorter and more direct.",
            "Rephrase this so it is easier for a beginner to understand.",
            "Change the tone of this message to be more friendly.",
            "Convert this passive voice into active voice.",
            "Tighten up this product description.",
            "Rewrite this in British English.",
            "Improve the flow of this introduction.",
            "Turn these bullet points into flowing prose.",
            "Edit this for clarity without changing the meaning.",
            "Make this headline more compelling.",
        ],
    ),
    (
        "extract-structure",
        &[
            "Extract all the dates and amounts from this invoice.",
            "Pull out the names of every person mentioned in this text.",
            "Convert this table into JSON.",
            "List the action items from these meeting notes.",
            "Find every email address in this document.",
            "Parse this log line into structured fields.",
            "Return the product names and prices as a CSV.",
            "Identify the company names in this article.",
            "Extract the key-value pairs from this configuration text.",
            "Give me the phone numbers from this contact list.",
            "Turn this recipe into a structured ingredient list.",
            "Pull the citations out of this bibliography.",
        ],
    ),
    (
        "translate",
        &[
            "Translate this paragraph into Spanish.",
            "How do you say 'thank you very much' in Japanese?",
            "Translate this technical documentation into German.",
            "Convert this English email into formal French.",
            "What is the Dutch word for 'appointment'?",
            "Translate these subtitles into Portuguese.",
            "Render this poem in Italian keeping the rhyme.",
            "Translate this from Mandarin into English.",
            "How would a native speaker phrase this in Korean?",
            "Give me the Swedish translation of this menu.",
            "Translate this contract clause into Polish.",
            "What does this Russian sentence mean in English?",
        ],
    ),
    (
        "creative-writing",
        &[
            "Write a short story about a lighthouse keeper who finds a message.",
            "Compose a poem about autumn in the style of Frost.",
            "Write a limerick about a cat who learns to code.",
            "Draft an opening scene for a science fiction novel.",
            "Invent a backstory for a fantasy blacksmith character.",
            "Write song lyrics about leaving a small town.",
            "Create a bedtime story about a brave little tugboat.",
            "Write a monologue for a villain who believes he is right.",
            "Draft a humorous wedding speech for my brother.",
            "Write a haiku about the first snow.",
            "Come up with a dramatic plot twist for this outline.",
            "Write a fable with a moral about patience.",
        ],
    ),
    (
        "factual-qa",
        &[
            "What is the capital of Australia?",
            "Who wrote the novel Brave New World?",
            "When did the Berlin Wall fall?",
            "How tall is Mount Kilimanjaro?",
            "What is the chemical symbol for tungsten?",
            "Which planet has the most moons?",
            "Who won the World Cup in 1998?",
            "What year was the printing press invented?",
            "How many bones are in the adult human body?",
            "What is the largest desert on Earth?",
            "Who painted the Night Watch?",
            "What is the speed of light in a vacuum?",
        ],
    ),
];

fn main() {
    let mut corpus = load("no_robots.json");
    corpus.extend(load("gsm8k.json"));
    println!(
        "corpus: {} prompts\nmodel: {MODEL} (cap {CAP})\n",
        corpus.len()
    );

    let model = StaticModel::from_pretrained(MODEL, None, None, None).expect("load model");

    let texts: Vec<String> = corpus.iter().map(|r| r.prompt.clone()).collect();
    let mut vectors = model.encode_with_args(&texts, Some(CAP), 1024);
    for v in vectors.iter_mut() {
        normalise(v);
    }

    // -----------------------------------------------------------------
    // 1. What structure is actually there?
    // -----------------------------------------------------------------
    println!("=== unsupervised: k-means, k={K} ===");
    println!("(what the embedding separates when nobody imposes a taxonomy)\n");
    let (centroids, assignment) = kmeans(&vectors, K, 25);

    for (c, centre) in centroids.iter().enumerate() {
        let members: Vec<usize> = assignment
            .iter()
            .enumerate()
            .filter(|(_, a)| **a == c)
            .map(|(i, _)| i)
            .collect();
        if members.is_empty() {
            continue;
        }
        // Which human labels landed here — the cluster's meaning, in the
        // dataset's own vocabulary rather than ours.
        let mut labels: HashMap<&str, usize> = HashMap::new();
        for &i in &members {
            *labels.entry(corpus[i].category.as_str()).or_default() += 1;
        }
        let mut label_counts: Vec<(&str, usize)> = labels.into_iter().collect();
        label_counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let composition: Vec<String> = label_counts
            .iter()
            .take(3)
            .map(|(l, n)| format!("{l} {:.0}%", *n as f64 / members.len() as f64 * 100.0))
            .collect();

        // Cohesion: mean similarity to the centroid. A loose cluster is one
        // whose "class" would be a poor routing target however it is named.
        let cohesion: f32 = members
            .iter()
            .map(|&i| cosine(&vectors[i], centre))
            .sum::<f32>()
            / members.len() as f32;

        let mut nearest: Vec<(usize, f32)> = members
            .iter()
            .map(|&i| (i, cosine(&vectors[i], centre)))
            .collect();
        nearest.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        println!(
            "cluster {c}: n={} cohesion={cohesion:.3}  [{}]",
            members.len(),
            composition.join(", ")
        );
        for (i, _) in nearest.iter().take(3) {
            let p: String = corpus[*i].prompt.chars().take(88).collect();
            println!("    {}", p.replace('\n', " "));
        }
    }

    // -----------------------------------------------------------------
    // 2. A candidate taxonomy, scored the way the product would score it.
    // -----------------------------------------------------------------
    println!("\n=== supervised: ten candidate routing classes ===");
    let mut class_names: Vec<&str> = Vec::new();
    let mut class_vectors: Vec<Vec<Vec<f32>>> = Vec::new();
    for (name, seeds) in TAXONOMY {
        let owned: Vec<String> = seeds.iter().map(|s| s.to_string()).collect();
        let mut v = model.encode_with_args(&owned, Some(CAP), 1024);
        for x in v.iter_mut() {
            normalise(x);
        }
        class_names.push(name);
        class_vectors.push(v);
    }
    let class_centroids: Vec<Vec<f32>> = class_vectors
        .iter()
        .map(|vs| mean(&vs.iter().collect::<Vec<_>>()))
        .collect();

    // The diagnostic that decides whether a taxonomy is viable at all. Two
    // classes whose centroids sit close together cannot be separated by any
    // threshold — the fix is to merge or redefine them, not to tune.
    println!("\ncentroid similarity (higher = more confusable):");
    print!("{:>18}", "");
    for n in &class_names {
        print!("{:>7}", &n[..n.len().min(6)]);
    }
    println!();
    let mut collisions: Vec<(f32, &str, &str)> = Vec::new();
    for (i, a) in class_centroids.iter().enumerate() {
        print!("{:>18}", class_names[i]);
        for (j, b) in class_centroids.iter().enumerate() {
            let s = cosine(a, b);
            print!("{s:>7.2}");
            if i < j {
                collisions.push((s, class_names[i], class_names[j]));
            }
        }
        println!();
    }
    collisions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    println!("\nmost confusable pairs:");
    for (s, a, b) in collisions.iter().take(5) {
        println!("    {a} <-> {b}: {s:.3}");
    }

    // Leave-one-out over the seeds themselves: can each class even recognise
    // its own examples once they are excluded from its centroid?
    println!("\nleave-one-out over the seed prompts:");
    let mut correct_total = 0usize;
    let mut n_total = 0usize;
    let mut margins_by_class: Vec<(f32, f32)> = Vec::new();
    for (ci, vs) in class_vectors.iter().enumerate() {
        let mut correct = 0usize;
        let mut margin_sum = 0.0f32;
        let mut worst = f32::INFINITY;
        for (vi, v) in vs.iter().enumerate() {
            let mut scored: Vec<(usize, f32)> = class_vectors
                .iter()
                .enumerate()
                .map(|(cj, other)| {
                    let subset: Vec<&Vec<f32>> = other
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !(cj == ci && *i == vi))
                        .map(|(_, x)| x)
                        .collect();
                    (cj, cosine(v, &mean(&subset)))
                })
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            if scored[0].0 == ci {
                correct += 1;
            }
            let m = scored[0].1 - scored[1].1;
            margin_sum += m;
            worst = worst.min(m);
        }
        correct_total += correct;
        n_total += vs.len();
        margins_by_class.push((margin_sum / vs.len() as f32, worst));
        println!(
            "    {:<18} {}/{} correct   mean margin {:.3}   worst {:.3}",
            class_names[ci],
            correct,
            vs.len(),
            margin_sum / vs.len() as f32,
            worst
        );
    }
    println!(
        "    overall {correct_total}/{n_total} ({:.1}%)",
        correct_total as f64 / n_total as f64 * 100.0
    );

    // How the taxonomy carves up real traffic, and how confidently.
    println!("\nassigning the real corpus to these classes (margin floor 0.10):");
    let mut assigned: HashMap<&str, usize> = HashMap::new();
    let mut below_floor = 0usize;
    for v in &vectors {
        let mut scored: Vec<(usize, f32)> = class_centroids
            .iter()
            .enumerate()
            .map(|(i, c)| (i, cosine(v, c)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        if scored[0].1 - scored[1].1 < 0.10 {
            below_floor += 1;
        } else {
            *assigned.entry(class_names[scored[0].0]).or_default() += 1;
        }
    }
    let mut rows: Vec<(&str, usize)> = assigned.into_iter().collect();
    rows.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (name, n) in rows {
        println!(
            "    {:<18} {:>6} ({:.1}%)",
            name,
            n,
            n as f64 / vectors.len() as f64 * 100.0
        );
    }
    println!(
        "    {:<18} {:>6} ({:.1}%) — falls through to the next rule",
        "below floor",
        below_floor,
        below_floor as f64 / vectors.len() as f64 * 100.0
    );

    // -----------------------------------------------------------------
    // 3. The same ten-ish classes, seeded from REAL prompts instead of
    //    invented ones.
    // -----------------------------------------------------------------
    //
    // The run above scores 74% on its own seeds and rejects 78% of real
    // traffic, which is a suspiciously bad result for a model that hits 98.7%
    // on coding-vs-other. The hypothesis: the seeds are the problem, not the
    // classes. They are tidy one-line imperatives, while real prompts arrive
    // with pasted articles, stack traces and context attached — so seed and
    // traffic sit in different regions and every similarity comes out low.
    //
    // If that is right, seeding from real traffic should lift both accuracy and
    // coverage sharply, and the product requirement is "seed from your own
    // logs", not "write good examples".
    println!("\n=== seeded from real prompts instead of invented ones ===");
    const SEEDS_PER_CLASS: usize = 12;
    let mut by_label: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, row) in corpus.iter().enumerate() {
        by_label.entry(row.category.as_str()).or_default().push(i);
    }
    let mut real_names: Vec<&str> = by_label
        .iter()
        .filter(|(_, v)| v.len() >= SEEDS_PER_CLASS * 4)
        .map(|(k, _)| *k)
        .collect();
    real_names.sort();

    // First N of each label as seeds, the rest as the evaluation set — a
    // held-out split, so this measures generalisation and not memory.
    let mut real_centroids: Vec<(&str, Vec<f32>)> = Vec::new();
    let mut eval: Vec<(usize, &str)> = Vec::new();
    for name in &real_names {
        let idxs = &by_label[name];
        let seeds: Vec<&Vec<f32>> = idxs[..SEEDS_PER_CLASS]
            .iter()
            .map(|&i| &vectors[i])
            .collect();
        real_centroids.push((name, mean(&seeds)));
        for &i in &idxs[SEEDS_PER_CLASS..] {
            eval.push((i, name));
        }
    }
    println!(
        "{} classes, {SEEDS_PER_CLASS} real seeds each, {} held-out prompts",
        real_centroids.len(),
        eval.len()
    );

    let mut collisions: Vec<(f32, &str, &str)> = Vec::new();
    for (i, (na, a)) in real_centroids.iter().enumerate() {
        for (nb, b) in real_centroids.iter().skip(i + 1) {
            collisions.push((cosine(a, b), na, nb));
        }
    }
    collisions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    println!("most confusable pairs:");
    for (s, a, b) in collisions.iter().take(4) {
        println!("    {a} <-> {b}: {s:.3}");
    }

    for floor in [0.0f32, 0.02, 0.05] {
        let mut tp: HashMap<&str, usize> = HashMap::new();
        let mut pred: HashMap<&str, usize> = HashMap::new();
        let mut act: HashMap<&str, usize> = HashMap::new();
        let mut kept = 0usize;
        for (i, actual) in &eval {
            let mut scored: Vec<(&str, f32)> = real_centroids
                .iter()
                .map(|(n, c)| (*n, cosine(&vectors[*i], c)))
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            if scored[0].1 - scored[1].1 < floor {
                continue;
            }
            kept += 1;
            *pred.entry(scored[0].0).or_default() += 1;
            *act.entry(actual).or_default() += 1;
            if scored[0].0 == *actual {
                *tp.entry(scored[0].0).or_default() += 1;
            }
        }
        let correct: usize = tp.values().sum();
        println!(
            "\n  floor {floor:.2}: coverage {:.0}%, accuracy {:.1}%",
            kept as f64 / eval.len() as f64 * 100.0,
            correct as f64 / kept.max(1) as f64 * 100.0
        );
        if floor == 0.0 {
            for name in &real_names {
                let t = *tp.get(name).unwrap_or(&0) as f64;
                let p = *pred.get(name).unwrap_or(&0) as f64;
                let a = *act.get(name).unwrap_or(&0) as f64;
                println!(
                    "    {:<12} precision {:>5.1}%  recall {:>5.1}%  (n={})",
                    name,
                    if p > 0.0 { t / p * 100.0 } else { 0.0 },
                    if a > 0.0 { t / a * 100.0 } else { 0.0 },
                    a as usize
                );
            }
        }
    }

    // -----------------------------------------------------------------
    // 4. Merge the region that collides into one class.
    // -----------------------------------------------------------------
    //
    // The run above says the failure is concentrated: Closed QA / Rewrite /
    // Classify / Extract / Summarize sit at centroid similarities of 0.80-0.86,
    // which is another way of saying they are one region of the space with five
    // names. All five are "here is some text, transform it" — the differences
    // are in the *instruction*, not the subject, and a bag-of-embeddings has no
    // way to see that.
    //
    // If the diagnosis is right, folding them into a single class should leave a
    // taxonomy whose every member is usable.
    println!("\n=== the same corpus with the colliding region merged ===");
    const MERGED: &[&str] = &["Closed QA", "Rewrite", "Classify", "Extract", "Summarize"];
    let merged_label = |c: &str| -> &'static str {
        if MERGED.contains(&c) {
            "text-transform"
        } else if c == "Coding" {
            "coding"
        } else if c == "Math" {
            "math"
        } else if c == "Chat" {
            "chat"
        } else if c == "Generation" {
            "generation"
        } else if c == "Open QA" {
            "factual-qa"
        } else {
            "brainstorm"
        }
    };

    let mut merged_by_label: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, row) in corpus.iter().enumerate() {
        merged_by_label
            .entry(merged_label(&row.category))
            .or_default()
            .push(i);
    }
    let mut merged_names: Vec<&str> = merged_by_label.keys().copied().collect();
    merged_names.sort();

    let mut merged_centroids: Vec<(&str, Vec<f32>)> = Vec::new();
    let mut merged_eval: Vec<(usize, &str)> = Vec::new();
    for name in &merged_names {
        let idxs = &merged_by_label[name];
        let seeds: Vec<&Vec<f32>> = idxs[..SEEDS_PER_CLASS]
            .iter()
            .map(|&i| &vectors[i])
            .collect();
        merged_centroids.push((name, mean(&seeds)));
        for &i in &idxs[SEEDS_PER_CLASS..] {
            merged_eval.push((i, name));
        }
    }
    println!(
        "{} classes, {} held-out prompts",
        merged_centroids.len(),
        merged_eval.len()
    );

    for floor in [0.0f32, 0.02, 0.05, 0.10] {
        let mut tp: HashMap<&str, usize> = HashMap::new();
        let mut pred: HashMap<&str, usize> = HashMap::new();
        let mut act: HashMap<&str, usize> = HashMap::new();
        let mut kept = 0usize;
        for (i, actual) in &merged_eval {
            let mut scored: Vec<(&str, f32)> = merged_centroids
                .iter()
                .map(|(n, c)| (*n, cosine(&vectors[*i], c)))
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            if scored[0].1 - scored[1].1 < floor {
                continue;
            }
            kept += 1;
            *pred.entry(scored[0].0).or_default() += 1;
            *act.entry(actual).or_default() += 1;
            if scored[0].0 == *actual {
                *tp.entry(scored[0].0).or_default() += 1;
            }
        }
        let correct: usize = tp.values().sum();
        println!(
            "\n  floor {floor:.2}: coverage {:.0}%, accuracy {:.1}%",
            kept as f64 / merged_eval.len() as f64 * 100.0,
            correct as f64 / kept.max(1) as f64 * 100.0
        );
        if floor == 0.0 || floor == 0.05 {
            for name in &merged_names {
                let t = *tp.get(name).unwrap_or(&0) as f64;
                let p = *pred.get(name).unwrap_or(&0) as f64;
                let a = *act.get(name).unwrap_or(&0) as f64;
                println!(
                    "    {:<15} precision {:>5.1}%  recall {:>5.1}%  (n={})",
                    name,
                    if p > 0.0 { t / p * 100.0 } else { 0.0 },
                    if a > 0.0 { t / a * 100.0 } else { 0.0 },
                    a as usize
                );
            }
        }
    }
}
