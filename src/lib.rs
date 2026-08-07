//! fastllm-proxy — a low-latency OpenAI-compatible gateway for multi-node LLM serving.

// Prompt classification for routing rules. Feature-gated because it embeds a
// model: a build that routes on caller, shape or headers alone should not carry
// one. See `src/classifier/mod.rs` for why it is two tiers.
#[cfg(feature = "classifier")]
pub mod classifier;
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
