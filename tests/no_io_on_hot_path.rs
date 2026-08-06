//! The property most likely to rot.
//!
//! The proxy's measured overhead against a real vLLM is currently zero
//! (83.2ms vs 83.8ms time-to-first-token) because the request path does no
//! I/O: authorisation reads a pre-flattened in-memory `Snapshot`
//! (`src/snapshot.rs`) instead of querying a database. Every feature after
//! this one — rate limits, token budgets, routing rules — adds a temptation
//! to slip one more lookup onto that path. This file is what makes that
//! temptation fail loudly instead of quietly costing milliseconds.
//!
//! Two independent guards, each narrow on purpose:
//!
//! 1. Compile-time signature checks that `Snapshot::authenticate` and
//!    `Principal::may_invoke` are plain synchronous `fn`s with no pool,
//!    client or file-handle parameter. This is the strong half: it does not
//!    just observe today's behaviour, it makes a whole class of regression
//!    (turning either into `async fn`, or threading a handle through them)
//!    fail to *compile*, for anyone, in any build, not just when this test
//!    happens to run. See the doc comment on each assertion for exactly what
//!    coercion failure looks like.
//!
//! 2. A source-level scan of `proxy::authorize` — the function `handle`
//!    calls on every request before it does anything else — asserting its
//!    body contains no `.await` and none of a short deny-list of I/O tokens
//!    (`sqlx`, `Pool`, `query(`, `fs::`, `TcpStream`, `reqwest`, `Client::`).
//!    This is the weak half and it is crude by construction: it is a
//!    textual grep, not a semantic analysis, so it can be defeated by
//!    renaming an import, wrapping a call in a helper, or moving the I/O one
//!    frame away from `authorize` itself. It exists only to catch the
//!    straightforward version of the regression — someone adding an
//!    `.await` or an obviously-named I/O call directly inside `authorize` —
//!    and it says nothing about `proxy_request`'s upstream HTTP call, which
//!    is real, intentional I/O that happens *after* authorisation and is
//!    out of scope for this guard.
//!
//! What neither guard catches: I/O added inside `Registry`/`Router`
//! (consulted after authorisation, on the same request), I/O hidden behind
//! a trait object or dynamic dispatch, or a regression introduced only in
//! `--role all`'s control-plane feature-gated code paths that never touches
//! `authorize` at all. This file protects the authorisation step
//! specifically, not "the proxy never blocks."

use fastllm_proxy::snapshot::{hash_key, AuthError, KeyEntry, Principal, Snapshot};
use std::collections::HashMap;
use std::time::SystemTime;

/// If `Snapshot::authenticate` becomes `async fn`, its type stops being
/// `fn(&Snapshot, &str, SystemTime) -> Result<&Principal, AuthError>` and
/// becomes `fn(&Snapshot, &str, SystemTime) -> impl Future<Output = ...>`
/// instead — a value of the old fn-pointer type can no longer be assigned
/// from it, and this line fails to compile with a type mismatch naming the
/// opaque `impl Future` type. Equally, a change that threads a `&Pool` or
/// `&HttpClient` through the signature changes the parameter list and the
/// same coercion fails. This is checked at compile time for every build of
/// this test binary, not just when `#[test]` bodies happen to run.
const _AUTHENTICATE_IS_SYNC_AND_TAKES_NO_HANDLE: for<'a> fn(
    &'a Snapshot,
    &str,
    SystemTime,
) -> Result<&'a Principal, AuthError> = Snapshot::authenticate;

/// Same reasoning as above for `Principal::may_invoke`: were it `async fn`,
/// this would try to coerce `fn(&Principal, &str) -> impl Future<Output =
/// bool>` into `fn(&Principal, &str) -> bool` and fail to compile.
const _MAY_INVOKE_IS_SYNC_AND_TAKES_NO_HANDLE: fn(&Principal, &str) -> bool = Principal::may_invoke;

/// Extract the source text of a top-level `fn <name>` from `src/proxy.rs`
/// by counting braces from its first `{`. Panics (failing the test loudly,
/// not silently) if the function cannot be found, so a rename of
/// `authorize` breaks this test rather than making it vacuously pass.
fn extract_fn_body(source: &str, fn_signature_prefix: &str) -> String {
    let start = source
        .find(fn_signature_prefix)
        .unwrap_or_else(|| panic!("could not find `{fn_signature_prefix}` in src/proxy.rs"));
    let after = &source[start..];
    let brace_start = after
        .find('{')
        .expect("function signature must be followed by a body");
    let mut depth = 0i32;
    let mut end = None;
    for (i, c) in after[brace_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(brace_start + i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end.expect("unbalanced braces while scanning function body");
    after[..end].to_string()
}

/// Crude, honest, and cheap: see the module doc comment for exactly what
/// this does and does not prove. It is a second, independent signal on top
/// of the compile-time checks above, not a replacement for them.
#[test]
fn authorize_body_contains_no_await_or_io_tokens() {
    let source = include_str!("../src/proxy.rs");
    let body = extract_fn_body(source, "fn authorize<'a>(");

    assert!(
        body.contains("snapshot.authenticate("),
        "sanity check failed: `authorize` no longer calls `snapshot.authenticate`; \
         either the function was rewritten (update this test to match) or the \
         extraction above grabbed the wrong span"
    );

    assert!(
        !body.contains(".await"),
        "authorize() now awaits something — authorisation must stay synchronous \
         and read only the in-memory snapshot, never I/O"
    );

    for forbidden in [
        "sqlx",
        "Pool",
        "query(",
        "fs::",
        "TcpStream",
        "reqwest",
        "Client::",
    ] {
        assert!(
            !body.contains(forbidden),
            "authorize() body contains `{forbidden}`, which looks like I/O on the \
             authorisation path — authorisation must read only the pre-flattened \
             in-memory Snapshot"
        );
    }
}

/// End-to-end sanity check that the surface asserted above actually behaves
/// as authorisation: a known key resolves to a principal whose grants are
/// enforced. This does not itself guard against I/O (the compile-time
/// assertions and the source scan above do); it just proves the functions
/// those guards are pinned to are the real ones, not dead code.
#[test]
fn authorisation_reads_only_the_snapshot() {
    // `Snapshot::for_test` is `#[cfg(test)]`-only, so unlike the unit tests
    // in `src/snapshot.rs`, this integration test builds the snapshot from
    // its public fields directly.
    let mut keys = HashMap::new();
    keys.insert(
        hash_key("sk-x"),
        KeyEntry {
            principal: 1,
            expires_at: None,
            disabled: false,
        },
    );
    let mut principals = HashMap::new();
    principals.insert(
        1,
        Principal {
            id: 1,
            name: "p".into(),
            allowed_models: ["m".to_string()].into_iter().collect(),
            allow_all: false,
            roles: std::collections::HashSet::new(),
            limits: None,
            budget: None,
        },
    );
    let snap = Snapshot {
        version: 1,
        keys,
        principals,
        models: vec![],
        virtual_models: HashMap::new(),
        open: false,
    };

    let p = snap.authenticate("sk-x", SystemTime::now()).unwrap();
    assert!(p.may_invoke("m"));
    assert!(!p.may_invoke("other-model"));
}
