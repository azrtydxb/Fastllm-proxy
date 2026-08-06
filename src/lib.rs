//! fastllm-proxy — a low-latency OpenAI-compatible gateway for multi-node LLM serving.

pub mod config;
#[cfg(feature = "control")]
pub mod control;
pub mod health;
pub mod multipart;
pub mod proxy;
pub mod registry;
pub mod router;
pub mod snapshot;
pub mod source;
pub mod state;
pub mod upstream;
pub mod usage;
