//! fastllm-proxy — a low-latency OpenAI-compatible gateway for multi-node LLM serving.

pub mod config;
pub mod health;
pub mod multipart;
pub mod proxy;
pub mod registry;
pub mod router;
pub mod state;
pub mod upstream;
