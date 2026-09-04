//! Semantic routing, end to end through the real binary.
//!
//! Unit tests in `src/classifier/tests.rs` pin the dispatch logic against
//! hand-written vectors. This proves the part they structurally cannot: that a
//! class defined through the admin API, embedded by the control plane and
//! shipped in a snapshot actually causes a real HTTP request to reach a
//! different model — and, just as importantly, that a deployment which defines
//! no classes behaves exactly as it did before the feature existed.
//!
//! The classification test is gated on the `classifier` feature: without it the
//! binary compiles fine and the CLI flag is still accepted, but `classify`
//! always returns `None`, so a rule naming a class can never match. Running
//! that assertion in a build with no classifier would fail for a reason that
//! has nothing to do with the code under test.
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
/// Downloaded into the HuggingFace cache (`HF_HOME`) on first use. In CI that
/// cache is restored by actions/cache — the runners are ephemeral pods, so
/// without it every run starts cold and pays the download at proxy startup.
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
        .track_suffix("frontend_models", "name", suffix)
        .track_suffix("prompt_classes", "name", suffix)
        .track_suffix("principals", "name", suffix)
        .track_suffix("api_keys", "name", suffix)
        .track_suffix("roles", "name", suffix)
        .track_suffix("permissions", "resource", suffix)
}

fn wait_healthy(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if !matches!(
            ureq::get(&format!("http://127.0.0.1:{port}/health")).call(),
            Err(ureq::Error::Transport(_))
        ) {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "fastllm-proxy on port {port} did not answer /health within {}s",
                timeout.as_secs()
            );
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
    .env("FASTLLM_ENCRYPTION_KEY", encryption_key())
    // One pool per spawned process against a shared 100-connection
    // server; the default of 8 exhausts it once enough tests run at once.
    .env("FASTLLM_DATABASE_MAX_CONNECTIONS", "2");
    if with_classifier {
        cmd.args(["--classifier-model", CLASSIFIER_MODEL]);
    }
    let child = cmd.spawn().expect("failed to spawn fastllm-proxy");
    let proc = Proc(child);
    // A classifier-enabled start may be paying the one-time model download
    // before the listener comes up; 60s only covers a warm cache.
    let timeout = if with_classifier {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(60)
    };
    wait_healthy(port, timeout);
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
    frontend_name: String,
    /// Only read by the classification test, which is gated on the
    /// `classifier` feature — without it, nothing routes to the coding model
    /// and there is nothing to assert against.
    #[cfg_attr(not(feature = "classifier"), allow(dead_code))]
    coding_base: String,
    default_base: String,
}

/// Two provider models on unreachable addresses, a frontend model in front, and
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
            "/admin/provider-models",
            serde_json::json!({"name": name, "description": "semantic routing e2e"}),
        );
        let id = m["id"].as_i64().unwrap();
        admin_post(
            admin_port,
            cookie,
            &format!("/admin/provider-models/{id}/backends"),
            serde_json::json!({"api_base": base, "upstream_model": name}),
        );
        ids.push(id);
    }

    let frontend_name = format!("auto-{suffix}");
    let vm = admin_post(
        admin_port,
        cookie,
        "/admin/frontend-models",
        serde_json::json!({"name": frontend_name}),
    );
    let vm_id = vm["id"].as_i64().unwrap();

    let principal = format!("sr-principal-{suffix}");
    let p = admin_post(
        admin_port,
        cookie,
        "/admin/principals",
        serde_json::json!({"name": principal}),
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
        &format!("/admin/frontend-models/{vm_id}/rules"),
        serde_json::json!({"position": 0, "class": format!("horticulture-{suffix}")}),
    );
    let rule_id = rule["id"].as_i64().unwrap();
    admin_post(
        admin_port,
        cookie,
        &format!("/admin/rules/{rule_id}/targets"),
        serde_json::json!({"provider_model_id": ids[0], "weight": 100, "position": 0}),
    );
    admin_post(
        admin_port,
        cookie,
        &format!("/admin/frontend-models/{vm_id}/defaults"),
        serde_json::json!({"provider_model_id": ids[1], "weight": 100, "position": 0}),
    );

    Fixture {
        key: k["key"].as_str().unwrap().to_string(),
        frontend_name,
        coding_base,
        default_base,
    }
}

/// Deliberately an unusual domain.
///
/// Classification scores against *every* class in the snapshot, and this suite
/// shares a database with other tests and with a live dev cluster that has real
/// `coding` and `chat` classes of its own. A test class named `coding` competes
/// with those and the winner is a coin flip; horticulture competes with nothing.
/// The point under test is that a class routes, not which words it contains.
/// A second, distant domain, so the negative case is deterministic.
///
/// Asserting that a prompt is *unclassified* is not: with only a couple of
/// classes defined, every prompt is nearest to one of them and a permissive
/// floor lets it through. Which is correct behaviour — a class set that covers
/// two topics will happily assign a third topic to whichever is closer. So the
/// negative case here is a prompt that classifies cleanly into a class **no
/// rule names**, which falls through for a reason the test controls.
#[cfg(feature = "classifier")]
const MARITIME_EXAMPLES: &[&str] = &[
    "How do I calculate great-circle distance between two ports?",
    "What draft can this vessel manage through the canal?",
    "When does demurrage start accruing on a bill of lading?",
    "Which incoterm puts the freight cost on the buyer?",
    "How is laytime counted for a voyage charter?",
    "What is the difference between a bareboat and a time charter?",
    "Which flag state registry suits a coastal cargo vessel?",
    "How do I read a vessel's stability booklet?",
];

#[cfg(feature = "classifier")]
const HORTICULTURE_EXAMPLES: &[&str] = &[
    "When should I prune my apple trees?",
    "What is the best mulch for a raised vegetable bed?",
    "How often should tomatoes be watered in a greenhouse?",
    "My hydrangeas have yellow leaves, what is wrong?",
    "Which cover crop should I sow before the frost?",
    "How do I propagate rosemary from cuttings?",
    "What pH does a blueberry bush need in the soil?",
    "When is the right time to divide perennials?",
];

/// Wait until each *named* class reports a centroid.
///
/// By name, not by count: every test in this file shares one database, so the
/// list contains classes other tests created, and "two classes are routable"
/// can be satisfied by somebody else's two while this test's own are still
/// being embedded. That is what made this flaky roughly one run in four.
///
/// Not a fixed sleep either — the control plane rebuilds on its own schedule,
/// and how long that takes depends on what else is running.
#[cfg_attr(not(feature = "classifier"), allow(dead_code))]
fn wait_for_routable_classes(admin_port: u16, cookie: &str, names: &[String]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let classes = admin_get(admin_port, cookie, "/admin/prompt-classes");
        let empty = Vec::new();
        let all = classes.as_array().unwrap_or(&empty);
        let missing: Vec<&String> = names
            .iter()
            .filter(|n| {
                !all.iter().any(|c| {
                    c["name"] == serde_json::Value::String((*n).clone()) && c["routable"] == true
                })
            })
            .collect();
        if missing.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("classes never became routable within 30s: {missing:?} of {classes}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn wait_for_routing(port: u16, key: &str, model: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let body = routed_to(port, key, model, "hello");
        if !body.contains("model_not_found") && !body.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("frontend model {model} never became routable: {body}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The headline: a coding prompt and a chat prompt, identical in every way the
/// other rule conditions can see, reach different models.
#[cfg(feature = "classifier")]
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

    // One class, seeded with example prompts. This is the whole of the
    // configuration — no training step, no model to fine-tune.
    for (name, examples) in [
        (format!("horticulture-{suffix}"), HORTICULTURE_EXAMPLES),
        (format!("maritime-{suffix}"), MARITIME_EXAMPLES),
    ] {
        admin_post(
            admin_port,
            &cookie,
            "/admin/prompt-classes",
            serde_json::json!({
                "name": name, "tier": "fast", "min_margin": 0.02, "examples": examples,
            }),
        );
    }
    wait_for_routing(port, &fx.key, &fx.frontend_name);
    wait_for_routable_classes(
        admin_port,
        &cookie,
        &[
            format!("horticulture-{suffix}"),
            format!("maritime-{suffix}"),
        ],
    );

    // The probe is one of the class's own seed prompts, on purpose.
    //
    // This test exists to prove the *plumbing* — that a class defined through
    // the admin API, embedded by the control plane, shipped in a snapshot and
    // named by a rule actually steers a live HTTP request. Whether the model
    // generalises to held-out prompts is a different question, measured over
    // ~21k labelled examples in `docs/classifier/measurements.md`, and asserting it here
    // would make this test's result depend on which other classes happen to
    // exist in a shared database.
    let matched = routed_to(port, &fx.key, &fx.frontend_name, HORTICULTURE_EXAMPLES[0]);
    assert!(
        matched.contains(&fx.coding_base),
        "a horticulture question should reach the class's model, got: {matched}"
    );

    // Classifies cleanly as `maritime`, which no rule names — so it falls
    // through to the default for a reason this test controls, rather than by
    // hoping nothing matches.
    let unmatched = routed_to(port, &fx.key, &fx.frontend_name, MARITIME_EXAMPLES[0]);
    assert!(
        unmatched.contains(&fx.default_base),
        "a prompt outside the class should fall through to the default, got: {unmatched}"
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

    // The content is irrelevant here — this test has no classifier, so nothing
    // is ever embedded or compared. What it must *not* be is anything close to
    // the horticulture class the sibling test defines: every proxy in this file
    // embeds every class in the shared database, and two classes seeded with
    // neighbouring prompts produce neighbouring centroids, which collapses the
    // margin between them below any floor. That is real product behaviour —
    // `POST /admin/prompt-classes/evaluate` exists to surface it — but here it
    // made one test fail because of another test's data.
    admin_post(
        admin_port,
        &cookie,
        "/admin/prompt-classes",
        serde_json::json!({
            "name": format!("conveyancing-{suffix}"),
            "examples": [
                "What searches are required before exchanging contracts?",
                "Who pays the stamp duty on a leasehold assignment?",
                "How long is the standard completion period after exchange?",
                "What is an indemnity policy for a missing building regulation?",
            ],
        }),
    );
    wait_for_routing(port, &fx.key, &fx.frontend_name);
    // No classifier here, so the class never becomes routable — wait for the
    // snapshot to carry the class at all, then assert it does not match.
    std::thread::sleep(Duration::from_secs(3));

    let body = routed_to(
        port,
        &fx.key,
        &fx.frontend_name,
        "Why does my Python script raise a KeyError?",
    );
    assert!(
        body.contains(&fx.default_base),
        "with no classifier the class rule must not match, and the default must serve: {body}"
    );

    // And the operator can see why, rather than having to guess.
    //
    // Looked up by name, not by position: the list is every class in the
    // database, including ones other tests in this file created.
    let classes = admin_get(admin_port, &cookie, "/admin/prompt-classes");
    let name = format!("conveyancing-{suffix}");
    let c = classes
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == serde_json::Value::String(name.clone()))
        .unwrap_or_else(|| panic!("{name} missing from {classes}"));
    assert_eq!(c["examples"].as_i64(), Some(4));
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
