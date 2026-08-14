//! Talking to the Kubernetes API about *this* deployment, when an operator
//! runs it.
//!
//! # Why the control plane knows about the operator at all
//!
//! Everything an operator configures splits in two. Models, keys, grants,
//! routing rules and budgets are rows in Postgres, and the UI has always been
//! able to change them. The *shape of the deployment* — image, replicas,
//! autoscaling, the selection policy — is a `FastllmProxy` spec, and until
//! now the only way to change it was `kubectl edit`. That is a strange seam to
//! put in front of somebody who is already looking at a management UI.
//!
//! So: when a `FastllmProxy` manages this process, the admin API can read and
//! patch it, and the UI grows one screen. When nothing does — `File` mode, a
//! Helm install, a laptop — none of this is reachable and the UI never shows
//! it. Detection is not a guess: the operator injects
//! `FASTLLM_OPERATOR_RESOURCE` into the control plane it creates, so a
//! deployment nobody operates has nothing to find.
//!
//! # Why a hand-rolled client
//!
//! This makes two calls: `GET` one resource and `PATCH` one resource. A
//! Kubernetes client crate would bring a discovery layer, a watch machinery,
//! a schema for every builtin type and its own TLS stack into an image whose
//! whole point is being small and starting fast — for two URLs. In-cluster
//! configuration is three files and two environment variables, all of them
//! specified and stable.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use std::sync::Arc;
use std::time::Duration;

use crate::upstream::{Config as UpstreamConfig, Upstream};

const TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
const CA_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";

/// The `FastllmProxy` this process belongs to, and how to reach the API
/// server holding it.
pub struct Operator {
    pub namespace: String,
    pub name: String,
    host: String,
    client: Arc<Upstream>,
}

impl Operator {
    /// `Some` only inside a cluster, under an operator-managed deployment,
    /// with a service-account token mounted.
    ///
    /// Every one of those is a separate reason to be absent, and none of them
    /// is an error: a Helm install is not a broken operator install, it is a
    /// different install. The UI asks once and hides the screen.
    pub fn from_env() -> Option<Self> {
        let resource = std::env::var("FASTLLM_OPERATOR_RESOURCE").ok()?;
        let (namespace, name) = resource.split_once('/')?;
        let host = std::env::var("KUBERNETES_SERVICE_HOST").ok()?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
        if !std::path::Path::new(TOKEN_PATH).exists() {
            tracing::warn!(
                "FASTLLM_OPERATOR_RESOURCE is set but no service-account token is mounted; \
                 the deployment screen will be unavailable"
            );
            return None;
        }
        let client = match api_client() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "could not build a Kubernetes API client");
                return None;
            }
        };
        Some(Self {
            namespace: namespace.to_string(),
            name: name.to_string(),
            // Bracketed for IPv6, which a cluster service address can be.
            host: if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            },
            client,
        })
    }

    fn url(&self) -> String {
        format!(
            "https://{}/apis/fastllm.io/v1alpha1/namespaces/{}/fastllmproxies/{}",
            self.host, self.namespace, self.name
        )
    }

    /// The service-account token, read per call rather than cached.
    ///
    /// Projected tokens are rotated in place — an hour is a common expiry —
    /// and a control plane that read it once at startup would start getting
    /// 401s after the first rotation, on a screen nobody opens often enough
    /// to notice quickly. A file read is nothing next to the round trip it
    /// authenticates.
    fn token(&self) -> Result<String> {
        Ok(std::fs::read_to_string(TOKEN_PATH)
            .context("reading the service-account token")?
            .trim()
            .to_string())
    }

    async fn send(&self, req: Request<Full<Bytes>>) -> Result<serde_json::Value> {
        let resp = tokio::time::timeout(Duration::from_secs(10), self.client.request(req))
            .await
            .map_err(|_| anyhow::anyhow!("the Kubernetes API did not answer within 10s"))??;
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("reading the Kubernetes API response body: {e}"))?
            .to_bytes();
        if !status.is_success() {
            // The API server's own message is far more useful than anything
            // this could invent — "is forbidden: User ... cannot patch" names
            // the missing RBAC rule exactly.
            let detail = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).to_string());
            bail!("Kubernetes API returned {status}: {detail}");
        }
        Ok(serde_json::from_slice(&body)?)
    }

    pub async fn get(&self) -> Result<serde_json::Value> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(self.url())
            .header(
                hyper::header::AUTHORIZATION,
                format!("Bearer {}", self.token()?),
            )
            .header(hyper::header::ACCEPT, "application/json")
            .body(Full::default())?;
        self.send(req).await
    }

    /// Merge-patch the spec.
    ///
    /// A merge patch rather than a full apply: the UI edits a handful of
    /// fields and must not have an opinion about the rest of a resource
    /// somebody may also be managing in Git. Sending back a whole spec would
    /// silently revert anything added since it was read.
    pub async fn patch_spec(&self, spec: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::to_vec(&serde_json::json!({ "spec": spec }))?;
        let req = Request::builder()
            .method(Method::PATCH)
            .uri(self.url())
            .header(
                hyper::header::AUTHORIZATION,
                format!("Bearer {}", self.token()?),
            )
            .header(hyper::header::CONTENT_TYPE, "application/merge-patch+json")
            .header(hyper::header::ACCEPT, "application/json")
            .body(Full::new(Bytes::from(body)))?;
        self.send(req).await
    }
}

/// A client that trusts the cluster CA and nothing else.
///
/// Deliberately its own client rather than the shared upstream pool: that one
/// carries whatever roots `--ca-bundle` set for reaching *backends*, and the
/// API server is not a backend. Mixing them would either widen what a backend
/// connection trusts or make reaching the API server depend on an unrelated
/// flag.
fn api_client() -> Result<Arc<Upstream>> {
    let pem = std::fs::read(CA_PATH).context("reading the cluster CA bundle")?;
    let mut roots = rustls::RootCertStore::empty();
    let mut cursor = std::io::Cursor::new(pem);
    for cert in rustls_pemfile::certs(&mut cursor) {
        roots.add(cert?)?;
    }
    if roots.is_empty() {
        bail!("the mounted cluster CA bundle contains no certificates");
    }
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(Upstream::new(
        UpstreamConfig {
            // One screen, one operator at a time: this needs a connection,
            // not a pool.
            max_idle_per_host: 2,
            idle_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
        },
        tls,
    )))
}

/// The fields the UI may change, translated into a merge patch.
///
/// An allowlist, and short on purpose. Everything here is a number or an
/// enumeration whose effect is a rollout the operator already knows how to
/// sequence. Deliberately absent:
///
/// - **The Secret references.** Repointing `encryptionKey` from a web form is
///   how a deployment loses every stored upstream credential; the CRD refuses
///   the edit outright and this refuses to ask.
/// - **`serviceType` and ingress.** Making the gateway reachable from outside
///   the cluster is a network decision, not a preference, and one that should
///   leave a trail in whatever manages the cluster.
/// - **`bootstrap`.** Rewriting it would reset an admin password from a page
///   that password protects.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct DeploymentEdit {
    pub image: Option<String>,
    pub replicas: Option<i32>,
    pub policy: Option<String>,
    pub upstream_timeout: Option<u32>,
    pub workers: Option<u32>,
    pub pool_max_idle: Option<u32>,
    pub autoscaling_enabled: Option<bool>,
    pub autoscaling_min_replicas: Option<i32>,
    pub autoscaling_max_replicas: Option<i32>,
    pub autoscaling_target_cpu: Option<i32>,
}

impl DeploymentEdit {
    /// `None` when the request would change nothing — a patch that sets no
    /// field still bumps the resource's generation and starts a reconcile, so
    /// an empty edit is refused rather than performed.
    pub fn into_patch(self) -> Option<serde_json::Value> {
        let mut spec = serde_json::Map::new();
        let mut proxy = serde_json::Map::new();
        let mut autoscaling = serde_json::Map::new();

        if let Some(v) = self.image {
            spec.insert("image".into(), v.into());
        }
        if let Some(v) = self.replicas {
            proxy.insert("replicas".into(), v.into());
        }
        if let Some(v) = self.policy {
            proxy.insert("policy".into(), v.into());
        }
        if let Some(v) = self.upstream_timeout {
            proxy.insert("upstreamTimeout".into(), v.into());
        }
        if let Some(v) = self.workers {
            proxy.insert("workers".into(), v.into());
        }
        if let Some(v) = self.pool_max_idle {
            proxy.insert("poolMaxIdle".into(), v.into());
        }
        if let Some(v) = self.autoscaling_enabled {
            autoscaling.insert("enabled".into(), v.into());
        }
        if let Some(v) = self.autoscaling_min_replicas {
            autoscaling.insert("minReplicas".into(), v.into());
        }
        if let Some(v) = self.autoscaling_max_replicas {
            autoscaling.insert("maxReplicas".into(), v.into());
        }
        if let Some(v) = self.autoscaling_target_cpu {
            autoscaling.insert("targetCpuUtilizationPercentage".into(), v.into());
        }

        if !autoscaling.is_empty() {
            proxy.insert("autoscaling".into(), autoscaling.into());
        }
        if !proxy.is_empty() {
            spec.insert("proxy".into(), proxy.into());
        }
        (!spec.is_empty()).then(|| spec.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The patch has to use the CRD's own spelling. A key the schema does not
    /// declare is not an error anywhere: a structural schema *prunes* it, so
    /// the API server answers 200, stores nothing, and the UI reports success
    /// while changing nothing at all.
    ///
    /// That is not hypothetical. This sent `targetCPUUtilizationPercentage`,
    /// which reads correctly to anyone who knows what HPAs call it, while the
    /// CRD generated from `AutoscalingSpec` declares
    /// `targetCpuUtilizationPercentage`. It was found by reading a real
    /// resource back, not by a test — hence
    /// `every_patched_key_exists_in_the_generated_crd` below.
    #[test]
    fn the_patch_uses_the_field_names_the_crd_declares() {
        let patch = DeploymentEdit {
            replicas: Some(4),
            upstream_timeout: Some(90),
            pool_max_idle: Some(32),
            autoscaling_target_cpu: Some(60),
            ..Default::default()
        }
        .into_patch()
        .expect("a patch");
        assert_eq!(patch["proxy"]["replicas"], 4);
        assert_eq!(patch["proxy"]["upstreamTimeout"], 90);
        assert_eq!(patch["proxy"]["poolMaxIdle"], 32);
        assert_eq!(
            patch["proxy"]["autoscaling"]["targetCpuUtilizationPercentage"],
            60
        );
    }

    /// A merge patch touches only the keys it carries. Sending a whole `proxy`
    /// object would blank every field the UI does not know about — the
    /// scheduling block, the ingress, the annotations somebody set in Git.
    #[test]
    fn an_edit_carries_only_what_changed() {
        let patch = DeploymentEdit {
            image: Some("ghcr.io/x/y:v2".into()),
            ..Default::default()
        }
        .into_patch()
        .expect("a patch");
        assert_eq!(patch["image"], "ghcr.io/x/y:v2");
        assert!(patch.get("proxy").is_none(), "{patch}");
    }

    /// Every key this can emit, against the schema the operator installs.
    ///
    /// The CRD is generated from the operator's Rust types and committed at
    /// `operator/deploy/crd.yaml`; this walks the same file the cluster gets.
    /// Cheap, and it closes the one failure mode a merge patch has: a
    /// misspelled key is silently dropped rather than rejected.
    #[test]
    fn every_patched_key_exists_in_the_generated_crd() {
        let crd = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/operator/deploy/crd.yaml"
        ))
        .expect("operator/deploy/crd.yaml");

        // Every field the type can set, at once.
        let patch = DeploymentEdit {
            image: Some("x".into()),
            replicas: Some(1),
            policy: Some("cacheAffinity".into()),
            upstream_timeout: Some(1),
            workers: Some(1),
            pool_max_idle: Some(1),
            autoscaling_enabled: Some(true),
            autoscaling_min_replicas: Some(1),
            autoscaling_max_replicas: Some(1),
            autoscaling_target_cpu: Some(1),
        }
        .into_patch()
        .expect("a patch");

        let mut keys = Vec::new();
        collect_keys(&patch, &mut keys);
        for key in keys {
            assert!(
                crd.contains(&format!("{key}:")),
                "the CRD declares no {key:?}; a merge patch would prune it silently"
            );
        }
    }

    fn collect_keys(value: &serde_json::Value, out: &mut Vec<String>) {
        if let Some(map) = value.as_object() {
            for (k, v) in map {
                out.push(k.clone());
                collect_keys(v, out);
            }
        }
    }

    #[test]
    fn an_empty_edit_is_not_a_patch() {
        assert!(DeploymentEdit::default().into_patch().is_none());
    }

    /// The absent fields are absent on purpose — see the type's doc comment.
    /// `deny_unknown_fields` is what makes that a rejection rather than a
    /// silent no-op, the same rule the rest of this admin API follows.
    #[test]
    fn a_field_this_screen_must_not_change_is_refused_rather_than_ignored() {
        for body in [
            r#"{"encryption_key":{"name":"s","key":"k"}}"#,
            r#"{"service_type":"LoadBalancer"}"#,
            r#"{"bootstrap":{"name":"admin"}}"#,
        ] {
            assert!(
                serde_json::from_str::<DeploymentEdit>(body).is_err(),
                "{body} should be refused"
            );
        }
    }
}
