//! The control plane: everything that touches Postgres.
//!
//! Feature-gated so a `--role=proxy` build links no database driver at all.

pub mod api;
pub mod auth;
pub mod build;
pub mod db;
pub mod gcp;
pub mod import;
pub mod k8s;
pub mod pricing;
pub mod reconcile;
pub mod secrets;
#[cfg(test)]
pub mod test_support;
pub mod tls;
pub mod ui;
