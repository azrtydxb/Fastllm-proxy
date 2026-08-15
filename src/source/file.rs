//! Builds a snapshot from the YAML config, giving `File` mode the same
//! authorisation model as the control plane rather than a second code path.

use crate::config::FileConfig;
use crate::snapshot::{hash_key, BackendDef, KeyEntry, ModelDef, Principal, Snapshot};
use crate::source::SnapshotSource;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub struct FileSource {
    path: PathBuf,
    /// `--master-key`/`general_settings.master_key`, merged into every
    /// snapshot this source produces.
    ///
    /// The legacy key is not part of the YAML schema parsed below — it is
    /// bolted on at the CLI layer — and it lives here rather than at the call
    /// site because *every* fetch has to carry it: the initial load, a SIGHUP
    /// reload, and every poll tick. Applying it only to the first snapshot
    /// would silently drop a deployment's only credential the moment the
    /// underlying config changed. `File` mode is the only mode where the flag
    /// does anything at all; see `main.rs` for why it is inert elsewhere.
    legacy_master_key: Option<String>,
}

impl FileSource {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            legacy_master_key: None,
        }
    }

    pub fn with_legacy_master_key(mut self, key: Option<String>) -> Self {
        self.legacy_master_key = key;
        self
    }
}

impl SnapshotSource for FileSource {
    async fn fetch(&self, have: Option<u64>) -> anyhow::Result<Option<Snapshot>> {
        let raw = std::fs::read(&self.path)?;
        // The file has no version of its own, so its content hash is the
        // version. Identical content is not a change, which is the same rule
        // the config watcher already follows.
        let digest = hash_key(&String::from_utf8_lossy(&raw));
        let mut version_bytes = [0u8; 8];
        version_bytes.copy_from_slice(&digest[..8]);
        let version = u64::from_le_bytes(version_bytes);
        if have == Some(version) {
            return Ok(None);
        }

        let cfg: FileConfig = serde_yaml::from_slice(&raw)?;
        cfg.validate()?;

        let mut models: Vec<ModelDef> = Vec::new();
        for entry in &cfg.model_list {
            let name = entry.model_name.clone();
            let backend = BackendDef {
                api_base: entry
                    .litellm_params
                    .api_base
                    .trim_end_matches('/')
                    .to_string(),
                upstream_model: entry.litellm_params.upstream_model(&name),
                api_key: entry.litellm_params.effective_api_key(),
                // These four are not `Default::default()`, though they were.
                // The comment that used to sit here said this format describes
                // OpenAI-compatible upstreams only — but `LitellmParams` parses
                // all four, and `Registry::build` honours them on the other
                // `File`-mode path. The same YAML therefore described a
                // different backend depending on which path read it, and the
                // one that dropped them was silent about it.
                protocol: entry.litellm_params.protocol_or_default(),
                auth_header: entry
                    .litellm_params
                    .auth_header
                    .clone()
                    .unwrap_or_else(|| "authorization".to_string()),
                auth_scheme: entry.litellm_params.auth_scheme_or_default(),
                default_max_tokens: entry.litellm_params.default_max_tokens,
            };
            match models.iter_mut().find(|m| m.name == name) {
                Some(m) => m.backends.push(backend),
                None => models.push(ModelDef {
                    name,
                    policy: entry
                        .policy
                        .as_deref()
                        .and_then(crate::router::Policy::parse),
                    // File mode has no place to configure a TTL, so caching is
                    // off — the same as any model that has not asked for it.
                    cache_ttl: None,
                    // File mode has nowhere to declare it, so it is unknown
                    // rather than unlimited — see `ModelDef::context_length`.
                    context_length: None,
                    backends: vec![backend],
                }),
            }
        }

        let mut keys = HashMap::new();
        let mut principals = HashMap::new();
        for (i, k) in cfg.auth.keys.iter().enumerate() {
            let id = i as u64 + 1;
            let allow_all = k.models.iter().any(|m| m == "*");
            principals.insert(
                id,
                Principal {
                    id,
                    name: k.name.clone(),
                    allowed_models: k.models.iter().filter(|m| *m != "*").cloned().collect(),
                    allow_all,
                    // `File` mode has nowhere to define an MCP server, so
                    // there is nothing to grant — see `Snapshot.mcp_servers`.
                    allowed_mcp: HashSet::new(),
                    allow_all_mcp: false,
                    allowed_agents: HashSet::new(),
                    allow_all_agents: false,
                    // `File` mode's `auth.keys` schema has no role concept —
                    // routing rules that match by role are a control-plane
                    // (P1) feature and `File` mode carries no virtual models
                    // to evaluate them against anyway.
                    roles: HashSet::new(),
                    limits: k.limits.map(|l| crate::limiter::Limits {
                        requests_per_min: l.requests_per_min,
                        tokens_per_min: l.tokens_per_min,
                    }),
                    budget: k.budget.map(|b| crate::snapshot::Budget {
                        tokens_total: Some(b.tokens_total),
                        tokens_used: b.tokens_used,
                        // File mode has no pricing to compute a cost from, so a
                        // config-file budget stays a token budget.
                        cost_total_micros: None,
                        cost_used_micros: 0,
                    }),
                },
            );
            keys.insert(
                hash_key(&k.key),
                KeyEntry {
                    principal: id,
                    expires_at: k.expires_at.as_deref().map(parse_rfc3339).transpose()?,
                    disabled: false,
                },
            );
        }

        let open = keys.is_empty();
        let mut snap = Snapshot {
            // `File` mode has nowhere to store example prompts, so semantic
            // routing is a control-plane feature the same way virtual models
            // are.
            prompt_classes: Vec::new(),
            // Same reason as `prompt_classes`: a YAML file has nowhere to
            // store a server, and no grants to authorise reaching one.
            mcp_servers: HashMap::new(),
            a2a_agents: HashMap::new(),
            // `File` mode has no place to mark one, and its whole point is a
            // single YAML that says exactly what it says.
            fallback_model: None,
            version,
            keys,
            principals,
            models,
            // `File` mode has no database to store rules in — see the field
            // doc comment on `Snapshot::virtual_models`.
            virtual_models: HashMap::new(),
            open,
        };
        if let Some(key) = &self.legacy_master_key {
            snap.add_legacy_master_key(key);
        }
        Ok(Some(snap))
    }
}

fn parse_rfc3339(s: &str) -> anyhow::Result<std::time::SystemTime> {
    Ok(humantime::parse_rfc3339(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[tokio::test]
    async fn a_config_without_auth_produces_an_open_snapshot() {
        // Matches today's behaviour: no master key means no authentication.
        let f = write_config(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://h:8000/v1 }\n",
        );
        let snap = FileSource::new(f.path().into())
            .fetch(None)
            .await
            .unwrap()
            .unwrap();
        assert!(snap.open);
        assert_eq!(snap.models.len(), 1);
        assert_eq!(snap.models[0].name, "m");
    }

    #[tokio::test]
    async fn keys_and_grants_are_read_from_the_auth_section() {
        let f = write_config(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://h:8000/v1 }\n\
             auth:\n  keys:\n    - key: sk-eval\n      name: eval\n      models: [m]\n",
        );
        let snap = FileSource::new(f.path().into())
            .fetch(None)
            .await
            .unwrap()
            .unwrap();
        assert!(!snap.open);
        let p = snap
            .authenticate("sk-eval", std::time::SystemTime::now())
            .unwrap();
        assert_eq!(p.name, "eval");
        assert!(p.may_invoke("m"));
        assert!(!p.may_invoke("other"));
    }

    #[tokio::test]
    async fn a_star_grant_means_every_model() {
        let f = write_config(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://h:8000/v1 }\n\
             auth:\n  keys:\n    - key: sk-admin\n      name: admin\n      models: ['*']\n",
        );
        let snap = FileSource::new(f.path().into())
            .fetch(None)
            .await
            .unwrap()
            .unwrap();
        let p = snap
            .authenticate("sk-admin", std::time::SystemTime::now())
            .unwrap();
        assert!(p.allow_all);
        assert!(p.may_invoke("anything"));
    }

    /// Not just the first load: a `File`-mode reload that dropped the legacy
    /// key would take a deployment's only credential away mid-flight.
    #[tokio::test]
    async fn the_legacy_master_key_is_merged_into_every_fetch() {
        let f = write_config(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://h:8000/v1 }\n",
        );
        let src =
            FileSource::new(f.path().into()).with_legacy_master_key(Some("sk-legacy".to_string()));
        let first = src.fetch(None).await.unwrap().unwrap();
        assert!(first
            .authenticate("sk-legacy", std::time::SystemTime::now())
            .is_ok());

        std::fs::write(
            f.path(),
            "model_list:\n  - model_name: b\n    litellm_params: { api_base: http://h:8000/v1 }\n",
        )
        .unwrap();
        let second = src.fetch(Some(first.version)).await.unwrap().unwrap();
        assert_eq!(second.models[0].name, "b");
        assert!(second
            .authenticate("sk-legacy", std::time::SystemTime::now())
            .is_ok());
    }

    #[tokio::test]
    async fn an_unchanged_file_reports_no_new_snapshot() {
        let f = write_config(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://h:8000/v1 }\n",
        );
        let src = FileSource::new(f.path().into());
        let first = src.fetch(None).await.unwrap().unwrap();
        assert!(src.fetch(Some(first.version)).await.unwrap().is_none());
    }
}
