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
    /// Mirrors the control plane's `limits` table (P2) for `File` mode,
    /// where there is no database to store it in. Absent means unlimited —
    /// see `crate::limiter::Limits::is_unlimited`.
    #[serde(default)]
    pub limits: Option<LimitsConfig>,
    /// Mirrors the control plane's `budgets` table (P3) for `File` mode,
    /// same rationale as `limits` above. `File` mode has no reconciliation
    /// loop reporting usage back into this file, so `tokens_used` here is
    /// static for the life of the process — useful for testing enforcement
    /// or for a hand-managed "this key already spent N tokens elsewhere"
    /// starting point, but it will not advance on its own the way a
    /// control-plane-backed budget does from real `POST /usage` traffic.
    #[serde(default)]
    pub budget: Option<BudgetConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct LimitsConfig {
    #[serde(default)]
    pub requests_per_min: Option<u32>,
    #[serde(default)]
    pub tokens_per_min: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct BudgetConfig {
    pub tokens_total: u64,
    #[serde(default)]
    pub tokens_used: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    /// The name clients address this model by.
    pub model_name: String,
    pub litellm_params: LitellmParams,
    /// How to choose between this model's replicas, overriding `--policy`.
    /// Ignored by LiteLLM, like everything else under `fastllm:`.
    ///
    /// Here as well as in the database because `File` mode and the control
    /// plane must describe the same deployment: a field one path honours and
    /// the other drops is how the same YAML came to mean two different things
    /// once already (see `LitellmParams`'s note about the four backend
    /// fields).
    #[serde(default)]
    pub policy: Option<String>,
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

    // The four below exist on the control plane's `model_backends` table and
    // were unreachable from a YAML file, so `File` mode could not describe a
    // native backend, an Azure-style key header, or an Anthropic backend's
    // required token cap. The same deployment was configurable one way and not
    // the other, which is the kind of gap nobody notices until they hit it.
    /// Wire format this upstream speaks: `openai` (default), `anthropic`,
    /// `gemini`.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Header the key is sent in. Defaults to `authorization`; Azure OpenAI
    /// wants `api-key`.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Prefix before the key. Defaults to `Bearer`; an empty string sends the
    /// key raw, which is what `api-key` and `x-api-key` expect.
    #[serde(default)]
    pub auth_scheme: Option<String>,
    /// Sent when the client names no limit. Required in practice for an
    /// Anthropic backend, which rejects a request without one.
    #[serde(default)]
    pub default_max_tokens: Option<u32>,
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
        // Transport prefixes first: these name *how* the backend is reached,
        // so what follows is the model's real name upstream. `openrouter/`
        // belongs here and what remains after it is an OpenRouter model id,
        // which is itself namespaced — `openrouter/anthropic/claude-sonnet-4`
        // must become `anthropic/claude-sonnet-4`, not `claude-sonnet-4`.
        // Only one prefix is ever stripped, which is what makes that work.
        for provider in TRANSPORT_PREFIXES {
            if let Some(rest) = raw.strip_prefix(provider) {
                if !rest.is_empty() {
                    return rest.to_string();
                }
            }
        }
        // A native prefix is stripped only when it names the wire format this
        // backend actually speaks. `anthropic/claude-sonnet-4` means two
        // different things depending on where it is pointed: to Anthropic it
        // is a model called `claude-sonnet-4`, and to OpenRouter it is a model
        // whose id is the whole string. Stripping unconditionally asks
        // OpenRouter for a model that does not exist.
        let protocol = self.protocol_or_default();
        for (prefix, native) in NATIVE_PREFIXES {
            if protocol == *native {
                if let Some(rest) = raw.strip_prefix(prefix) {
                    if !rest.is_empty() {
                        return rest.to_string();
                    }
                }
            }
        }
        raw.to_string()
    }

    /// Prefix for the credential header, as three distinct states.
    ///
    /// The distinction matters and was being lost:
    ///
    /// - **absent** — the ordinary case, `Authorization: Bearer <key>`.
    /// - **`""`** — send the key with no prefix at all, which is what
    ///   `api-key` (Azure) and `x-api-key` (Anthropic) require.
    /// - **anything else** — that prefix.
    ///
    /// Collapsing the first two turns every backend that did not mention
    /// `auth_scheme` into one that sends its key raw. `Registry::build` did
    /// exactly that, and nothing caught it because the backends exercised in
    /// tests either carry no credential or set the field explicitly.
    pub fn auth_scheme_or_default(&self) -> Option<String> {
        match self.auth_scheme.as_deref() {
            None => Some("Bearer".to_string()),
            Some("") => None,
            Some(scheme) => Some(scheme.to_string()),
        }
    }

    /// Upstream bearer token, with LiteLLM's placeholders normalised away.
    /// The wire format for this backend.
    ///
    /// An unrecognised name is rejected rather than silently treated as
    /// OpenAI: `protocol: anthropc` is a typo an operator wants to hear about
    /// at startup, not as a stream of confusing upstream 400s.
    pub fn protocol_or_default(&self) -> crate::protocol::Protocol {
        self.protocol
            .as_deref()
            .and_then(crate::protocol::Protocol::parse)
            .unwrap_or_default()
    }

    /// Whether `protocol` names something this build understands, for the
    /// startup check that turns a typo into an error.
    pub fn protocol_is_valid(&self) -> bool {
        self.protocol
            .as_deref()
            .is_none_or(|p| crate::protocol::Protocol::parse(p).is_some())
    }

    pub fn effective_api_key(&self) -> Option<String> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty() && *k != "not-needed" && *k != "none" && *k != "null")
            .map(str::to_string)
    }
}

/// Provider prefixes LiteLLM may prepend that are not part of the model name.
/// Prefixes that name how a backend is *reached*, not what the model is
/// called once the request arrives.
///
/// Stripping these is unambiguous: LiteLLM writes `openai/<model>` to say "an
/// OpenAI-compatible endpoint", and vLLM or SGLang expect the bare name.
/// `openrouter/` is the same statement about transport, and what follows it is
/// an OpenRouter model id — which is itself namespaced, so stripping exactly
/// one prefix leaves `anthropic/claude-sonnet-4` intact, which is what
/// OpenRouter is asked for.
///
/// Only a known prefix is stripped, so a model whose own name contains a
/// slash — `Qwen/Qwen3-8B` — survives.
const TRANSPORT_PREFIXES: &[&str] = &[
    "openai/",
    "hosted_vllm/",
    "vllm/",
    "openai_like/",
    "openrouter/",
];

/// Prefixes that name a wire format, stripped only when the backend speaks it.
///
/// `anthropic/claude-sonnet-4` is two different things depending on where it
/// points. Sent to Anthropic it is a model called `claude-sonnet-4` and the
/// prefix has to go. Sent to OpenRouter — or any other gateway that namespaces
/// its catalogue — the whole string is the id and stripping it asks for a
/// model that does not exist.
///
/// `azure/`, `vertex_ai/` and `bedrock/` are deliberately absent. They name
/// OpenAI-shaped endpoints, so the protocol cannot tell them apart from a
/// gateway's namespaced id, and guessing wrong breaks a working backend to
/// tidy up a name.
const NATIVE_PREFIXES: &[(&str, crate::protocol::Protocol)] = &[
    ("anthropic/", crate::protocol::Protocol::Anthropic),
    ("gemini/", crate::protocol::Protocol::Gemini),
];

#[cfg(test)]
mod auth_scheme_and_prefix_tests {
    use super::*;

    fn params(yaml: &str) -> LitellmParams {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// Absent is not the same as empty, and conflating them sends every
    /// credential without its `Bearer`.
    #[test]
    fn an_unmentioned_auth_scheme_is_bearer_and_an_empty_one_is_raw() {
        let absent = params("{ api_base: http://h/v1, api_key: sk-x }");
        assert_eq!(absent.auth_scheme_or_default(), Some("Bearer".into()));

        let empty = params("{ api_base: http://h/v1, api_key: sk-x, auth_scheme: \"\" }");
        assert_eq!(
            empty.auth_scheme_or_default(),
            None,
            "\"\" means send it raw"
        );

        let named = params("{ api_base: http://h/v1, api_key: sk-x, auth_scheme: Token }");
        assert_eq!(named.auth_scheme_or_default(), Some("Token".into()));
    }

    /// The same string means different things depending on where it points,
    /// and getting this wrong breaks a working backend rather than a broken
    /// one.
    #[test]
    fn a_native_prefix_is_stripped_only_when_the_backend_speaks_that_protocol() {
        // To Anthropic, `anthropic/claude-sonnet-4` is a model called
        // `claude-sonnet-4`.
        let native = params(
            "{ api_base: https://api.anthropic.com/v1, model: anthropic/claude-sonnet-4, \
               protocol: anthropic }",
        );
        assert_eq!(native.upstream_model("fallback"), "claude-sonnet-4");

        // To OpenRouter, the whole string is the model id. Stripping it asks
        // for a model that does not exist.
        let gateway =
            params("{ api_base: https://openrouter.ai/api/v1, model: anthropic/claude-sonnet-4 }");
        assert_eq!(
            gateway.upstream_model("fallback"),
            "anthropic/claude-sonnet-4",
            "an OpenRouter id must survive intact"
        );

        let gemini_native = params(
            "{ api_base: https://generativelanguage.googleapis.com, \
               model: gemini/gemini-2.0-flash, protocol: gemini }",
        );
        assert_eq!(gemini_native.upstream_model("fallback"), "gemini-2.0-flash");
    }

    /// Transport prefixes say how the backend is reached, so they always go —
    /// and exactly one goes, which is what leaves an OpenRouter id namespaced.
    #[test]
    fn a_transport_prefix_is_always_stripped_and_only_one_of_them() {
        for (raw, want) in [
            ("openai/Qwen/Qwen3-8B", "Qwen/Qwen3-8B"),
            ("hosted_vllm/Qwen/Qwen3-8B", "Qwen/Qwen3-8B"),
            (
                "openrouter/anthropic/claude-sonnet-4",
                "anthropic/claude-sonnet-4",
            ),
            ("openrouter/openai/gpt-4o", "openai/gpt-4o"),
            // Not a prefix at all: the org is part of the name.
            ("Qwen/Qwen3-8B", "Qwen/Qwen3-8B"),
        ] {
            let p = params(&format!("{{ api_base: http://h/v1, model: {raw} }}"));
            assert_eq!(p.upstream_model("fallback"), want, "for {raw}");
        }
    }

    /// `azure/` and friends name OpenAI-shaped endpoints, so nothing
    /// distinguishes them from a gateway namespace. Left alone deliberately.
    #[test]
    fn an_openai_shaped_provider_prefix_is_left_alone() {
        let p = params(
            "{ api_base: https://x.openai.azure.com/openai/deployments/gpt-4o, \
                          model: azure/gpt-4o, auth_header: api-key, auth_scheme: \"\" }",
        );
        assert_eq!(p.upstream_model("fallback"), "azure/gpt-4o");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(model: Option<&str>) -> LitellmParams {
        LitellmParams {
            protocol: None,
            auth_header: None,
            auth_scheme: None,
            default_max_tokens: None,
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
