//! Score candidate prompts against candidate class definitions.
//!
//! Written for a specific failure: an end-to-end test asserted that "What is
//! the tallest mountain in Europe?" would not be classified as `coding`, and it
//! was — but only on a clean database, so it passed locally against a dev
//! database holding other classes and failed in CI. Choosing test prompts by
//! intuition is how that happens. This prints the actual margins so the choice
//! is made from the numbers.

use model2vec_rs::model::StaticModel;

const CODING: &[&str] = &[
    "Write a Python function that merges two sorted lists.",
    "Why does this Rust code fail the borrow checker?",
    "My unit test throws NullPointerException on line 42, here is the stack trace.",
    "Add error handling to this async fetch call in TypeScript.",
    "Implement binary search in C without recursion.",
    "Write a Dockerfile for a Go service that listens on port 8080.",
    "Generate a SQL migration that adds a nullable timestamp column.",
    "Refactor this class to use dependency injection.",
];
// Exactly the lists `tests/semantic_routing.rs` seeds, so the numbers below are
// the ones that test will see. An earlier version of this file included the
// held-out prompt among the examples, which of course made it score well.
const CHAT: &[&str] = &[
    "What is the capital of France?",
    "Tell me a joke about computers.",
    "Who wrote Pride and Prejudice?",
    "Recommend a book about the history of the internet.",
    "What's a good name for a golden retriever puppy?",
    "How do you say thank you in Portuguese?",
    "Summarise the plot of Hamlet briefly.",
    "Give me three ideas for a team offsite.",
];
const CANDIDATES: &[&str] = &[
    "Why does my Python script raise a KeyError when I loop over this dict?",
    "My unit test throws NullPointerException, here is the stack trace.",
    "Write a bash script that rotates log files older than 7 days.",
    "What is the tallest mountain in Europe?",
    "Recommend a good recipe for banana bread.",
    "Who painted the Mona Lisa?",
    "Tell me about the history of jazz in New Orleans.",
    "What time do most shops close in Amsterdam?",
];

fn main() {
    let model = StaticModel::from_pretrained("minishlab/potion-code-16M", None, None, None)
        .expect("load model");
    let embed = |texts: &[&str]| -> Vec<Vec<f32>> {
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        let mut v = model.encode_with_args(&owned, Some(128), 1024);
        for x in v.iter_mut() {
            fastllm_proxy::vector::normalise(x);
        }
        v
    };
    let coding = fastllm_proxy::vector::centroid(&embed(CODING)).unwrap();
    let chat = fastllm_proxy::vector::centroid(&embed(CHAT)).unwrap();
    println!(
        "class centroids sit at cosine {:.3}\n",
        fastllm_proxy::vector::cosine(&coding, &chat)
    );
    println!(
        "{:<62} {:>8} {:>8} {:>8}  winner",
        "prompt", "coding", "chat", "margin"
    );
    for (text, v) in CANDIDATES.iter().zip(embed(CANDIDATES)) {
        let c = fastllm_proxy::vector::cosine(&v, &coding);
        let h = fastllm_proxy::vector::cosine(&v, &chat);
        let winner = if c > h { "coding" } else { "chat" };
        println!(
            "{:<62} {c:>8.3} {h:>8.3} {:>8.3}  {winner}",
            text.chars().take(60).collect::<String>(),
            (c - h).abs()
        );
    }
}
