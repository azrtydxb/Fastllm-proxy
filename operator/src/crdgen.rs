//! Prints the CRD, generated from the same Rust types the controller
//! reconciles.
//!
//! The manifest is committed rather than generated at install time — an
//! operator you install with `kubectl apply -f` should not need a Rust
//! toolchain — but it is generated rather than hand-written, so a field added
//! to the struct and forgotten in the YAML is impossible rather than merely
//! unlikely. `operator/tests/crd_is_current.rs` fails if the committed file
//! and this output disagree.
//!
//!   cargo run -p fastllm-operator --bin crdgen > operator/deploy/crd.yaml

fn main() {
    print!("{}", fastllm_operator::crd::manifest_yaml());
}
