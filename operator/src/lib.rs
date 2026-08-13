//! Kubernetes operator for fastllm-proxy.
//!
//! A library as well as two binaries so the CRD-drift test can reach the same
//! types the controller reconciles: an integration test cannot see inside a
//! binary crate, and generating the manifest from one set of types while
//! testing another would defeat the point of generating it.

pub mod crd;
pub mod resources;
