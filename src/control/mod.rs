//! The control plane: everything that touches Postgres.
//!
//! Feature-gated so a `--role=proxy` build links no database driver at all.

pub mod build;
pub mod db;
