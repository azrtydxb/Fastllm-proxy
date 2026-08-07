//! fastllm-proxy — a low-latency OpenAI-compatible gateway for multi-node LLM serving.

pub mod config;
#[cfg(feature = "control")]
pub mod control;
pub mod health;
pub mod limiter;
pub mod multipart;
pub mod protocol;
pub mod proxy;
pub mod reconcile;
pub mod registry;
pub mod router;
pub mod routing;
pub mod snapshot;
pub mod source;
pub mod state;
pub mod tail_buffer;
pub mod upstream;
pub mod usage;
