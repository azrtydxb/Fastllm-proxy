//! Configuration loading.
//!
//! The on-disk schema is deliberately a superset of the LiteLLM proxy config
//! that `sparkrun proxy start` generates, so a config written for LiteLLM can
//! be handed to fastllm-proxy unchanged:
//!
//! ```yaml
//! model_list:
//!   - model_name: Qwen/Qwen3-1.7B
//!     litellm_params:
//!       model: openai/Qwen/Qwen3-1.7B
//!       api_base: http://10.24.11.13:8000/v1
//!       api_key: not-needed
//! general_settings:
//!   master_key: sk-...
//! ```
//!
//! Two `model_list` entries sharing a `model_name` are replicas of the same
//! model and become a load-balanced pool. Entries whose `litellm_params.model`
//! names a *different* model than `model_name` are aliases: the client-facing
//! name differs from the name the upstream expects, and the request body is
//! rewritten on the way through.
//!
//! Anything under the optional `fastllm:` key tunes this proxy specifically and
//! is ignored by LiteLLM, so one file can drive either.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Root of the config file.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub model_list: Vec<ModelEntry>,
    #[serde(default)]
    pub general_settings: GeneralSettings,
    /// fastllm-proxy specific tuning. Ignored by LiteLLM.
    #[serde(default)]
    pub fastllm: FastllmSettings,
    /// Per-key RBAC for `File` mode. Absent means open, matching today's
    /// behaviour when no master key is set.
    #[serde(default)]
    pub auth: AuthConfig,
}

/// Keys for `File` mode. The control plane replaces this entirely; it exists
/// so a proxy with no control plane still has real authorisation rather than
/// one shared secret.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeyConfig {
    pub key: String,
    pub name: String,
    /// Model names, or `*` for every model.
    #[serde(default)]
    pub models: Vec<String>,
    /// RFC 3339, e.g. `2027-01-01T00:00:00Z`.
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    /// The name clients address this model by.
    pub model_name: String,
    pub litellm_params: LitellmParams,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LitellmParams {
    /// Base URL of the upstream, e.g. `http://10.0.0.1:8000/v1`.
    pub api_base: String,
    /// Bearer token to present upstream. `not-needed` and empty are treated as absent.
    #[serde(default)]
    pub api_key: Option<String>,
    /// LiteLLM-style provider-qualified name, e.g. `openai/Qwen/Qwen3-1.7B`.
    /// The provider prefix is stripped; what remains is the model name sent
    /// upstream. Absent means "same as `model_name`".
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GeneralSettings {
    #[serde(default)]
    pub master_key: Option<String>,
}

/// Tuning knobs specific to this proxy.
///
/// Every field is optional; CLI flags override whatever lands here.
#[derive(Debug, Clone, Deserialize)]
pub struct FastllmSettings {
    /// Bytes of the raw request body hashed to derive the prefix-affinity key.
    #[serde(default = "default_prefix_bytes")]
    pub prefix_bytes: usize,
    /// A backend may hold its affinity while its in-flight count is within
    /// `min_inflight + max(balance_abs, min_inflight * balance_rel)`.
    #[serde(default = "default_balance_abs")]
    pub balance_abs: usize,
    #[serde(default = "default_balance_rel")]
    pub balance_rel: f64,
    /// Slots in the direct-mapped prefix-affinity cache. Rounded up to a power of two.
    #[serde(default = "default_affinity_slots")]
    pub affinity_slots: usize,
    /// Consecutive health-check failures before a backend is taken out of rotation.
    #[serde(default = "default_unhealthy_after")]
    pub unhealthy_after: u32,
}

impl Default for FastllmSettings {
    fn default() -> Self {
        Self {
            prefix_bytes: default_prefix_bytes(),
            balance_abs: default_balance_abs(),
            balance_rel: default_balance_rel(),
            affinity_slots: default_affinity_slots(),
            unhealthy_after: default_unhealthy_after(),
        }
    }
}

fn default_prefix_bytes() -> usize {
    2048
}
fn default_balance_abs() -> usize {
    8
}
fn default_balance_rel() -> f64 {
    1.5
}
fn default_affinity_slots() -> usize {
    65536
}
fn default_unhealthy_after() -> u32 {
    2
}

impl FileConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: FileConfig = serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        for entry in &self.model_list {
            if entry.model_name.trim().is_empty() {
                bail!("model_list entry has an empty model_name");
            }
            let base = entry.litellm_params.api_base.trim();
            if !(base.starts_with("http://") || base.starts_with("https://")) {
                bail!(
                    "model {}: api_base {:?} must start with http:// or https://",
                    entry.model_name,
                    base
                );
            }
        }
        if self.fastllm.balance_rel < 1.0 {
            bail!("fastllm.balance_rel must be >= 1.0");
        }
        Ok(())
    }
}

impl LitellmParams {
    /// Model name to send upstream, with any `provider/` prefix stripped.
    ///
    /// LiteLLM qualifies names as `openai/<model>`; vLLM and SGLang expect the
    /// bare name. Only a known provider prefix is stripped, so a model whose
    /// own name contains a slash (`Qwen/Qwen3-1.7B`) survives intact.
    pub fn upstream_model(&self, fallback: &str) -> String {
        let Some(raw) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return fallback.to_string();
        };
        for provider in PROVIDER_PREFIXES {
            if let Some(rest) = raw.strip_prefix(provider) {
                if !rest.is_empty() {
                    return rest.to_string();
                }
            }
        }
        raw.to_string()
    }

    /// Upstream bearer token, with LiteLLM's placeholders normalised away.
    pub fn effective_api_key(&self) -> Option<String> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty() && *k != "not-needed" && *k != "none" && *k != "null")
            .map(str::to_string)
    }
}

/// Provider prefixes LiteLLM may prepend that are not part of the model name.
const PROVIDER_PREFIXES: &[&str] = &["openai/", "hosted_vllm/", "vllm/", "openai_like/"];

#[cfg(test)]
mod tests {
    use super::*;

    fn params(model: Option<&str>) -> LitellmParams {
        LitellmParams {
            api_base: "http://localhost:8000/v1".into(),
            api_key: None,
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn strips_only_provider_prefix() {
        assert_eq!(
            params(Some("openai/Qwen/Qwen3-1.7B")).upstream_model("x"),
            "Qwen/Qwen3-1.7B"
        );
        // A bare org/model name must not lose its org.
        assert_eq!(
            params(Some("Qwen/Qwen3-1.7B")).upstream_model("x"),
            "Qwen/Qwen3-1.7B"
        );
    }

    #[test]
    fn falls_back_to_model_name() {
        assert_eq!(params(None).upstream_model("fallback"), "fallback");
        assert_eq!(params(Some("  ")).upstream_model("fallback"), "fallback");
    }

    #[test]
    fn placeholder_api_keys_are_dropped() {
        let mut p = params(None);
        p.api_key = Some("not-needed".into());
        assert_eq!(p.effective_api_key(), None);
        p.api_key = Some("sk-real".into());
        assert_eq!(p.effective_api_key(), Some("sk-real".into()));
    }

    #[test]
    fn rejects_non_http_api_base() {
        let cfg: FileConfig = serde_yaml::from_str(
            "model_list:\n  - model_name: m\n    litellm_params:\n      api_base: ftp://x/v1\n",
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn parses_a_sparkrun_generated_config() {
        let cfg: FileConfig = serde_yaml::from_str(
            r#"
model_list:
  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.24.11.13:8000/v1
      api_key: not-needed
  - model_name: Qwen/Qwen3-1.7B
    litellm_params:
      model: openai/Qwen/Qwen3-1.7B
      api_base: http://10.24.11.14:8000/v1
      api_key: not-needed
litellm_settings:
  drop_params: true
general_settings:
  master_key: sk-test
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.model_list.len(), 2);
        assert_eq!(cfg.general_settings.master_key.as_deref(), Some("sk-test"));
        // Unknown LiteLLM sections are tolerated, and fastllm defaults apply.
        assert_eq!(cfg.fastllm.prefix_bytes, 2048);
    }
}
