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
//! Four functions are guarded, one per stage of the request path a caller
//! actually waits on before the upstream call itself: `Snapshot::authenticate`
//! / `Principal::may_invoke` (authorisation), `Limiter::check` (P2 rate
//! limits), `VirtualModelDef::resolve` (P1 routing rules) and
//! `TailBuffer::push` (P3 usage accounting's hot-path side — the read-back
//! in `TailBuffer::extract_usage` runs once at end of stream, off the
//! per-frame path, and is deliberately not part of what this file pins).
//! Each gets the same two independent guards, narrow on purpose:
//!
//! 1. A compile-time signature check that the function is a plain
//!    synchronous `fn` with no pool, client or file-handle parameter. This
//!    is the strong half: it does not just observe today's behaviour, it
//!    makes a whole class of regression (turning the function `async fn`,
//!    or threading a handle through its signature) fail to *compile*, for
//!    anyone, in any build, not just when this test happens to run. See the
//!    doc comment on each assertion for exactly what coercion failure looks
//!    like. This is the part that actually bites — a signature check cannot
//!    be defeated by renaming an import or moving a call one frame away the
//!    way the source scan below can.
//!
//! 2. A source-level scan of the function's body, asserting it contains no
//!    `.await` and none of a short deny-list of I/O tokens (`sqlx`, `Pool`,
//!    `query(`, `fs::`, `TcpStream`, `reqwest`, `Client::`). This is the
//!    weak half and it is crude by construction: it is a textual grep, not
//!    a semantic analysis, so it can be defeated by renaming an import,
//!    wrapping a call in a helper, or moving the I/O one frame away from the
//!    guarded function itself. It exists only to catch the straightforward
//!    version of the regression — someone adding an `.await` or an
//!    obviously-named I/O call directly inside the function — and it says
//!    nothing about intentional I/O that happens in a *caller* of the
//!    guarded function (`proxy_request`'s upstream HTTP call after
//!    authorisation, `control::reconcile`'s HTTP round trip that only ever
//!    calls `Limiter::drain_counts`/`apply_allowances`, never `check`).
//!
//! What none of these guards catch: I/O added inside a function these four
//! call into (`Registry`/`Router`, consulted after authorisation and after
//! routing resolves, on the same request), I/O hidden behind a trait object
//! or dynamic dispatch, or a regression introduced only in `--role all`'s
//! control-plane feature-gated code paths that never touches any of these
//! four functions at all. This file protects these four specific functions,
//! not "the proxy never blocks."

use fastllm_proxy::limiter::{Decision, Limiter, Limits};
use fastllm_proxy::registry::Registry;
use fastllm_proxy::routing::{RequestFacts, VirtualModelDef};
use fastllm_proxy::snapshot::{hash_key, AuthError, KeyEntry, Principal, Snapshot};
use fastllm_proxy::tail_buffer::TailBuffer;
use std::collections::HashMap;
use std::time::{Instant, SystemTime};

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

/// Same reasoning again for `Limiter::check` (P2 rate limits, `src/limiter.rs`):
/// were it `async fn`, or were `&self` swapped for something backed by a
/// database handle, this coercion fails to compile.
const _LIMITER_CHECK_IS_SYNC_AND_TAKES_NO_HANDLE: fn(
    &Limiter,
    u64,
    &Limits,
    u32,
    Instant,
) -> Decision = Limiter::check;

/// Named purely so clippy's `type_complexity` lint (tuned for struct/field
/// types, not a one-off signature-pinning assertion like this) has
/// somewhere to point instead of at the const below. `#[allow(dead_code)]`:
/// the only "use" of this alias is inside a `const _NAME` binding, and the
/// leading-underscore exemption from the unused-code lint does not extend
/// to a type only that binding refers to.
#[allow(dead_code)]
type ResolveFn =
    for<'a> fn(&'a VirtualModelDef, &'a RequestFacts<'a>, u64, &'a Registry) -> Option<String>;

/// The same check for the candidate-list form, which is what `proxy_request`
/// actually calls now — `resolve` is a thin wrapper over it, so guarding only
/// the wrapper would leave the function doing the work unpinned.
///
/// `#[allow(dead_code)]` for the same reason as `ResolveFn` above: the
/// leading-underscore exemption on the binding does not reach the type it
/// refers to.
#[allow(dead_code)]
type ResolveCandidatesFn =
    for<'a> fn(&'a VirtualModelDef, &'a RequestFacts<'a>, u64, &'a Registry) -> Vec<String>;

/// Same reasoning again for `VirtualModelDef::resolve` (P1 routing rules,
/// `src/routing.rs`): were it `async fn`, or were `&Registry` swapped for a
/// pool/client, this coercion fails to compile. `&Registry` itself stays in
/// the signature deliberately — it is the in-memory backend-health view
/// this function reads, the routing equivalent of `Snapshot` above, not a
/// handle to anything that does I/O.
const _VIRTUAL_MODEL_RESOLVE_IS_SYNC_AND_TAKES_NO_HANDLE: ResolveFn = VirtualModelDef::resolve;
const _VIRTUAL_MODEL_RESOLVE_CANDIDATES_IS_SYNC_AND_TAKES_NO_HANDLE: ResolveCandidatesFn =
    VirtualModelDef::resolve_candidates;

/// Same reasoning again for `TailBuffer::push` (P3 usage accounting's
/// per-frame side, `src/tail_buffer.rs`): were it `async fn`, or were it
/// changed to accept a handle instead of the raw forwarded bytes, this
/// coercion fails to compile.
const _TAIL_BUFFER_PUSH_IS_SYNC_AND_TAKES_NO_HANDLE: fn(&mut TailBuffer, &[u8]) = TailBuffer::push;

/// Extract the source text of a top-level `fn <name>` from `source` by
/// counting braces from its first `{`. Panics (failing the test loudly, not
/// silently) if the function cannot be found, so a rename of the guarded
/// function breaks this test rather than making it vacuously pass.
fn extract_fn_body(source: &str, fn_signature_prefix: &str) -> String {
    let start = source
        .find(fn_signature_prefix)
        .unwrap_or_else(|| panic!("could not find `{fn_signature_prefix}` in the given source"));
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
/// of the compile-time checks above, not a replacement for them. Shared by
/// all four functions this file guards, so each `#[test]` below is just
/// "extract this body, then run the same scan" — the deny-list and its
/// caveats live in exactly one place.
fn assert_no_await_or_io_tokens(body: &str, fn_name: &str) {
    assert!(
        !body.contains(".await"),
        "{fn_name}() now awaits something — this function must stay synchronous \
         and do no I/O; see this file's module doc comment for which stage of \
         the request path that guarantee is for"
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
            "{fn_name}() body contains `{forbidden}`, which looks like I/O — this \
             function must read only pre-flattened in-memory state, never I/O"
        );
    }
}

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

    assert_no_await_or_io_tokens(&body, "authorize");
}

/// `Limiter::check` (P2 rate limits): a hash lookup plus a short
/// lock-protected refill-and-decrement, per the module doc comment on
/// `src/limiter.rs` — the same "no I/O, no allocation once the principal's
/// entry exists" guarantee `authorize` makes for authorisation.
#[test]
fn limiter_check_body_contains_no_await_or_io_tokens() {
    let source = include_str!("../src/limiter.rs");
    let body = extract_fn_body(source, "pub fn check(");

    assert!(
        body.contains("state_for"),
        "sanity check failed: `Limiter::check` no longer calls `state_for`; \
         either the function was rewritten (update this test to match) or the \
         extraction above grabbed the wrong span"
    );

    assert_no_await_or_io_tokens(&body, "Limiter::check");
}

/// `VirtualModelDef::resolve_candidates` (P1 routing rules): matches rules and
/// orders targets against the in-memory `Registry`/`RoutingRule` state built at
/// snapshot time, per `src/routing.rs`'s doc comments — reconciled *into*
/// the snapshot ahead of time, not looked up per request.
///
/// Scans the candidate-list form rather than `resolve`, which is now a
/// three-line wrapper over it: guarding the wrapper would say nothing about
/// the function that does the work.
#[test]
fn virtual_model_resolve_body_contains_no_await_or_io_tokens() {
    let source = include_str!("../src/routing.rs");
    let body = extract_fn_body(source, "pub fn resolve_candidates(");

    assert!(
        body.contains("order_candidates"),
        "sanity check failed: `VirtualModelDef::resolve_candidates` no longer calls \
         `order_candidates`; either the function was rewritten (update this \
         test to match) or the extraction above grabbed the wrong span"
    );

    assert_no_await_or_io_tokens(&body, "VirtualModelDef::resolve_candidates");
}

/// `TailBuffer::push` (P3 usage accounting's per-frame side): a bounded
/// `VecDeque` memcpy, per `src/tail_buffer.rs`'s module doc comment — the
/// one-time parse (`extract_usage`) is deliberately not this test's
/// concern, see the module doc comment above.
#[test]
fn tail_buffer_push_body_contains_no_await_or_io_tokens() {
    let source = include_str!("../src/tail_buffer.rs");
    let body = extract_fn_body(source, "pub fn push(&mut self, data: &[u8]) {");

    assert!(
        body.contains("self.buf"),
        "sanity check failed: `TailBuffer::push` no longer touches `self.buf`; \
         either the function was rewritten (update this test to match) or the \
         extraction above grabbed the wrong span"
    );

    assert_no_await_or_io_tokens(&body, "TailBuffer::push");
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
            allowed_mcp: Default::default(),
            allow_all_mcp: false,
            allowed_agents: Default::default(),
            allow_all_agents: false,
            roles: std::collections::HashSet::new(),
            limits: None,
            budget: None,
        },
    );
    let snap = Snapshot {
        prompt_classes: Vec::new(),
        mcp_servers: Default::default(),
        a2a_agents: Default::default(),
        fallback_model: None,
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
