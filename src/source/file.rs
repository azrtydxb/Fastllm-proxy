//! Builds a snapshot from the YAML config, giving `File` mode the same
//! authorisation model as the control plane rather than a second code path.

use crate::config::FileConfig;
use crate::snapshot::{hash_key, BackendDef, KeyEntry, ModelDef, Principal, Snapshot};
use crate::source::SnapshotSource;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub struct FileSource {
    path: PathBuf,
}

impl FileSource {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
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
                // The YAML config is the single-file, no-database deployment
                // shape; it describes OpenAI-compatible upstreams only. Native
                // providers need the per-backend protocol and auth columns the
                // control plane's schema carries, so they are configured
                // there rather than duplicated into this format.
                ..Default::default()
            };
            match models.iter_mut().find(|m| m.name == name) {
                Some(m) => m.backends.push(backend),
                None => models.push(ModelDef {
                    name,
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
                        tokens_total: b.tokens_total,
                        tokens_used: b.tokens_used,
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
        Ok(Some(Snapshot {
            // `File` mode has nowhere to store example prompts, so semantic
            // routing is a control-plane feature the same way virtual models
            // are.
            prompt_classes: Vec::new(),
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
        }))
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
