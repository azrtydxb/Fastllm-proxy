//! Semantic routing, end to end through the real binary.
//!
//! Unit tests in `src/classifier/tests.rs` pin the dispatch logic against
//! hand-written vectors. This proves the part they structurally cannot: that a
//! class defined through the admin API, embedded by the control plane and
//! shipped in a snapshot actually causes a real HTTP request to reach a
//! different model — and, just as importantly, that a deployment which defines
//! no classes behaves exactly as it did before the feature existed.
//!
//! Needs the classifier model, which is downloaded once and cached by
//! `model2vec-rs`:
//!
//! ```text
//! DATABASE_URL=$(cat /tmp/dburl) \
//!   cargo test --features "control classifier" --test semantic_routing -- --include-ignored
//! ```

use std::io::Read;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

#[path = "support/mod.rs"]
mod support;
use support::TestCleanup;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const PROXY_TOKEN: &str = "semantic-routing-e2e-token";
/// Downloaded once into the shared HuggingFace cache; every later run reuses it.
const CLASSIFIER_MODEL: &str = "minishlab/potion-code-16M";

fn encryption_key() -> String {
    "ee".repeat(32)
}

fn suffix(port: u16) -> String {
    format!(
        "{port}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn cleanup_for(suffix: &str) -> TestCleanup {
    TestCleanup::new()
        .track_suffix("models", "name", suffix)
        .track_suffix("virtual_models", "name", suffix)
        .track_suffix("prompt_classes", "name", suffix)
        .track_suffix("principals", "name", suffix)
        .track_suffix("api_keys", "name", suffix)
        .track_suffix("roles", "name", suffix)
        .track_suffix("permissions", "resource", suffix)
}

fn wait_healthy(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if !matches!(
            ureq::get(&format!("http://127.0.0.1:{port}/health")).call(),
            Err(ureq::Error::Transport(_))
        ) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("fastllm-proxy on port {port} did not answer /health within 60s");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn start(port: u16, admin_port: u16, database_url: &str, with_classifier: bool) -> Proc {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fastllm-proxy"));
    cmd.args([
        "--role",
        "all",
        "--port",
        &port.to_string(),
        "--admin-port",
        &admin_port.to_string(),
        "--snapshot-rebuild-interval",
        "1",
    ])
    .env("FASTLLM_DATABASE_URL", database_url)
    .env("FASTLLM_PROXY_TOKEN", PROXY_TOKEN)
    .env("FASTLLM_ENCRYPTION_KEY", encryption_key());
    if with_classifier {
        cmd.args(["--classifier-model", CLASSIFIER_MODEL]);
    }
    let child = cmd.spawn().expect("failed to spawn fastllm-proxy");
    let proc = Proc(child);
    wait_healthy(port);
    proc
}

fn admin_post(
    admin_port: u16,
    cookie: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    match ureq::post(&format!("http://127.0.0.1:{admin_port}{path}"))
        .set("cookie", cookie)
        .send_json(body.clone())
    {
        Ok(r) => r.into_json().unwrap_or(serde_json::Value::Null),
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            panic!("admin POST {path} with {body} failed: {code} {text}");
        }
        Err(e) => panic!("admin POST {path} failed: {e}"),
    }
}

fn admin_post_status(
    admin_port: u16,
    cookie: &str,
    path: &str,
    body: serde_json::Value,
) -> (u16, String) {
    match ureq::post(&format!("http://127.0.0.1:{admin_port}{path}"))
        .set("cookie", cookie)
        .send_json(body)
    {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("admin POST {path} failed: {e}"),
    }
}

fn admin_get(admin_port: u16, cookie: &str, path: &str) -> serde_json::Value {
    ureq::get(&format!("http://127.0.0.1:{admin_port}{path}"))
        .set("cookie", cookie)
        .call()
        .expect("admin GET")
        .into_json()
        .expect("JSON")
}

/// Every model in this test points at an address nothing is listening on, so a
/// request always ends as a 502 whose body names the backend that was chosen.
/// That is the assertion: which `api_base` appears tells us where routing sent
/// it, without needing a real inference server.
fn routed_to(port: u16, key: &str, model: &str, prompt: &str) -> String {
    let resp = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .set("authorization", &format!("Bearer {key}"))
        .send_json(serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
        }));
    let body = match resp {
        Ok(r) => {
            let mut s = String::new();
            let _ = r.into_reader().read_to_string(&mut s);
            s
        }
        Err(ureq::Error::Status(_, r)) => {
            let mut s = String::new();
            let _ = r.into_reader().read_to_string(&mut s);
            s
        }
        Err(e) => panic!("request failed: {e}"),
    };
    body
}

async fn grant_all(pool: &sqlx::PgPool, principal_id: i64, role_name: &str) {
    sqlx::query("INSERT INTO roles (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
        .bind(role_name)
        .execute(pool)
        .await
        .unwrap();
    let role_id: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE name = $1")
        .bind(role_name)
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions (role_id, permission_id)
         SELECT $1, id FROM permissions WHERE verb = 'model:invoke' AND resource = 'model/*'
         ON CONFLICT DO NOTHING",
    )
    .bind(role_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO principal_roles (principal_id, role_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(principal_id)
    .bind(role_id)
    .execute(pool)
    .await
    .unwrap();
}

struct Fixture {
    key: String,
    virtual_name: String,
    coding_base: String,
    default_base: String,
}

/// Two concrete models on unreachable addresses, a virtual model in front, and
/// a principal that may invoke everything.
async fn provision(pool: &sqlx::PgPool, admin_port: u16, cookie: &str, suffix: &str) -> Fixture {
    let coding_base = "http://127.0.0.1:9911/v1".to_string();
    let default_base = "http://127.0.0.1:9912/v1".to_string();
    let mut ids = Vec::new();
    for (n, base) in [
        ("coding-model", &coding_base),
        ("default-model", &default_base),
    ] {
        let name = format!("{n}-{suffix}");
        let m = admin_post(
            admin_port,
            cookie,
            "/admin/models",
            serde_json::json!({"name": name, "description": "semantic routing e2e"}),
        );
        let id = m["id"].as_i64().unwrap();
        admin_post(
            admin_port,
            cookie,
            &format!("/admin/models/{id}/backends"),
            serde_json::json!({"api_base": base, "upstream_model": name}),
        );
        ids.push(id);
    }

    let virtual_name = format!("auto-{suffix}");
    let vm = admin_post(
        admin_port,
        cookie,
        "/admin/virtual-models",
        serde_json::json!({"name": virtual_name}),
    );
    let vm_id = vm["id"].as_i64().unwrap();

    let principal = format!("sr-principal-{suffix}");
    let p = admin_post(
        admin_port,
        cookie,
        "/admin/principals",
        serde_json::json!({"name": principal, "roles": []}),
    );
    let principal_id = p["id"].as_i64().unwrap();
    grant_all(pool, principal_id, &format!("sr-role-{suffix}")).await;
    let k = admin_post(
        admin_port,
        cookie,
        "/admin/keys",
        serde_json::json!({"principal_id": principal_id, "name": format!("sr-key-{suffix}")}),
    );

    // Rule 0: anything classified `coding` goes to the coding model.
    // Default: everything else.
    let rule = admin_post(
        admin_port,
        cookie,
        &format!("/admin/virtual-models/{vm_id}/rules"),
        serde_json::json!({"position": 0, "class": format!("coding-{suffix}")}),
    );
    let rule_id = rule["id"].as_i64().unwrap();
    admin_post(
        admin_port,
        cookie,
        &format!("/admin/rules/{rule_id}/targets"),
        serde_json::json!({"model_id": ids[0], "weight": 100, "position": 0}),
    );
    admin_post(
        admin_port,
        cookie,
        &format!("/admin/virtual-models/{vm_id}/defaults"),
        serde_json::json!({"model_id": ids[1], "weight": 100, "position": 0}),
    );

    Fixture {
        key: k["key"].as_str().unwrap().to_string(),
        virtual_name,
        coding_base,
        default_base,
    }
}

const CODING_EXAMPLES: &[&str] = &[
    "Write a Python function that merges two sorted lists.",
    "Why does this Rust code fail the borrow checker?",
    "My unit test throws NullPointerException on line 42, here is the stack trace.",
    "Add error handling to this async fetch call in TypeScript.",
    "Implement binary search in C without recursion.",
    "Write a Dockerfile for a Go service that listens on port 8080.",
    "Generate a SQL migration that adds a nullable timestamp column.",
    "Refactor this class to use dependency injection.",
];

const CHAT_EXAMPLES: &[&str] = &[
    "What is the capital of France?",
    "Tell me a joke about computers.",
    "Who wrote Pride and Prejudice?",
    "Recommend a book about the history of the internet.",
    "What's a good name for a golden retriever puppy?",
    "How do you say thank you in Portuguese?",
    "Summarise the plot of Hamlet briefly.",
    "Give me three ideas for a team offsite.",
];

fn wait_for_routing(port: u16, key: &str, model: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let body = routed_to(port, key, model, "hello");
        if !body.contains("model_not_found") && !body.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("virtual model {model} never became routable: {body}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The headline: a coding prompt and a chat prompt, identical in every way the
/// other rule conditions can see, reach different models.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres and the classifier model"]
async fn a_prompt_class_routes_a_coding_question_away_from_the_default() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port) = (14911, 14912);
    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let admin_name = format!("sr-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start(port, admin_port, &database_url, true);
    let cookie = support::login_cookie(admin_port, &admin_name);
    let fx = provision(&pool, admin_port, &cookie, &suffix).await;

    // Two classes, seeded with example prompts. This is the whole of the
    // configuration — no training step, no model to fine-tune.
    for (name, examples) in [
        (format!("coding-{suffix}"), CODING_EXAMPLES),
        (format!("chat-{suffix}"), CHAT_EXAMPLES),
    ] {
        admin_post(
            admin_port,
            &cookie,
            "/admin/prompt-classes",
            serde_json::json!({
                "name": name,
                "tier": "fast",
                "min_margin": 0.02,
                "examples": examples,
            }),
        );
    }
    wait_for_routing(port, &fx.key, &fx.virtual_name);
    // Give the rebuilder a moment to publish centroids.
    std::thread::sleep(Duration::from_secs(3));

    let coding = routed_to(
        port,
        &fx.key,
        &fx.virtual_name,
        "Why does my Python script raise a KeyError when I loop over this dict?",
    );
    assert!(
        coding.contains(&fx.coding_base),
        "a coding question should reach the coding model, got: {coding}"
    );

    let chat = routed_to(
        port,
        &fx.key,
        &fx.virtual_name,
        "What is the tallest mountain in Europe?",
    );
    assert!(
        chat.contains(&fx.default_base),
        "a chat question should fall through to the default, got: {chat}"
    );
}

/// The property the whole two-tier design rests on, asserted from the outside:
/// a deployment with no classifier configured behaves exactly as it did before
/// the feature existed. A rule naming a class simply never matches, and the
/// next rule catches the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn without_a_classifier_a_class_rule_falls_through_rather_than_failing() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port) = (14921, 14922);
    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let admin_name = format!("sr-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    // Deliberately no --classifier-model.
    let _proc = start(port, admin_port, &database_url, false);
    let cookie = support::login_cookie(admin_port, &admin_name);
    let fx = provision(&pool, admin_port, &cookie, &suffix).await;

    admin_post(
        admin_port,
        &cookie,
        "/admin/prompt-classes",
        serde_json::json!({
            "name": format!("coding-{suffix}"),
            "examples": CODING_EXAMPLES,
        }),
    );
    wait_for_routing(port, &fx.key, &fx.virtual_name);
    std::thread::sleep(Duration::from_secs(3));

    let body = routed_to(
        port,
        &fx.key,
        &fx.virtual_name,
        "Why does my Python script raise a KeyError?",
    );
    assert!(
        body.contains(&fx.default_base),
        "with no classifier the class rule must not match, and the default must serve: {body}"
    );

    // And the operator can see why, rather than having to guess.
    let classes = admin_get(admin_port, &cookie, "/admin/prompt-classes");
    let c = classes.as_array().unwrap().iter().next().unwrap();
    assert_eq!(c["examples"].as_i64(), Some(CODING_EXAMPLES.len() as i64));
    assert_eq!(
        c["routable"].as_bool(),
        Some(false),
        "a class with no centroid must report itself unroutable"
    );
}

/// A refined class that refines nothing can never be reached, because
/// escalation keys entirely on the fast-tier class it names. Better a 400 at
/// write time than a class that looks configured and silently never matches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires postgres"]
async fn a_refined_class_must_name_what_it_refines() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port) = (14931, 14932);
    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");
    let admin_name = format!("sr-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start(port, admin_port, &database_url, false);
    let cookie = support::login_cookie(admin_port, &admin_name);

    let (status, text) = admin_post_status(
        admin_port,
        &cookie,
        "/admin/prompt-classes",
        serde_json::json!({"name": format!("arch-{suffix}"), "tier": "refined"}),
    );
    assert_eq!(status, 400, "{text}");
    assert!(
        text.contains("refines"),
        "the refusal must name the field: {text}"
    );

    let (status, text) = admin_post_status(
        admin_port,
        &cookie,
        "/admin/prompt-classes",
        serde_json::json!({
            "name": format!("arch-{suffix}"), "tier": "refined",
            "refines": ["coding"], "examples": ["Design a rate limiter for a payments API."],
        }),
    );
    assert_eq!(status, 201, "{text}");

    let (status, text) = admin_post_status(
        admin_port,
        &cookie,
        "/admin/prompt-classes",
        serde_json::json!({"name": format!("bad-tier-{suffix}"), "tier": "gpu"}),
    );
    assert_eq!(status, 400, "{text}");
    assert!(
        text.contains("fast"),
        "the refusal should list the valid tiers: {text}"
    );
}
