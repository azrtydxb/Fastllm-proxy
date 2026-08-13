//! The `FastllmProxy` custom resource.
//!
//! # Why a CRD rather than one more Helm chart
//!
//! The chart and the manifests both describe a deployment once, at apply
//! time. What they cannot do is *keep* describing it: a Deployment edited by
//! hand stays edited, a proxy pinned to an older image than its control plane
//! stays pinned, and nothing notices until a snapshot arrives with fields the
//! data plane does not understand.
//!
//! This resource is the desired state, and the controller's job is to keep
//! the cluster equal to it. The two properties worth having a controller for
//! are exactly the ones a chart cannot enforce after the fact:
//!
//! - **The two planes always run the same image.** They share a database
//!   schema. One `image` field here becomes both Deployments, so they cannot
//!   be pinned apart by anyone editing one of them.
//! - **Scaling means scaling the data plane.** `proxy.replicas` is a number;
//!   the control plane is not exposed as one, because a second control plane
//!   only races the first rebuilding snapshots.
//!
//! # Deliberately not modelled
//!
//! The database. A `FastllmProxy` points at a Secret holding a connection
//! string and never at a Postgres it operates: a database is a stateful
//! service with backup, failover and upgrade concerns that belong to whoever
//! runs it, and an operator that quietly owns one is an operator that can
//! quietly lose one.

use k8s_openapi::api::core::v1::ResourceRequirements;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where a value that must not be in git actually lives.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    /// Secret in the same namespace as this resource.
    pub name: String,
    /// Key within it.
    pub key: String,
}

/// The control plane. Its replica count is absent on purpose — see the type
/// doc on [`FastllmProxySpec`].
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControlSpec {
    /// Compute resources for the control-plane container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,

    /// TLS for the admin listener, which also serves `/snapshot`.
    ///
    /// `/snapshot` returns *decrypted* upstream credentials to anything
    /// holding the proxy token, so this is not decoration wherever a backend
    /// has a real credential. Name a Secret holding `tls.crt`, `tls.key` and
    /// `ca.crt` — a cert-manager `Certificate` produces exactly that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_secret_name: Option<String>,
}

/// The data plane: the part that scales.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxySpec {
    /// Gateway replicas.
    #[serde(default = "default_proxy_replicas")]
    pub replicas: i32,

    /// Backend selection policy.
    ///
    /// `cacheAffinity` keeps a shared prefix on the node already holding its
    /// KV cache. `lowestLatency` is for a pool whose members are not
    /// equivalent; `leastLoaded` for traffic with no prefix sharing at all.
    #[serde(default)]
    pub policy: Policy,

    /// Seconds to wait for upstream response *headers*. Does not bound
    /// generation — a long completion is not a hung request.
    #[serde(default = "default_upstream_timeout")]
    pub upstream_timeout: u32,

    /// Service type for the gateway. The admin Service is always ClusterIP,
    /// which is the point of having two.
    #[serde(default)]
    pub service_type: ServiceType,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

impl Default for ProxySpec {
    fn default() -> Self {
        Self {
            replicas: default_proxy_replicas(),
            policy: Policy::default(),
            upstream_timeout: default_upstream_timeout(),
            service_type: ServiceType::default(),
            resources: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum Policy {
    #[default]
    #[serde(rename = "cacheAffinity")]
    CacheAffinity,
    #[serde(rename = "leastLoaded")]
    LeastLoaded,
    #[serde(rename = "roundRobin")]
    RoundRobin,
    #[serde(rename = "lowestLatency")]
    LowestLatency,
}

impl Policy {
    /// The spelling the binary's `--policy` flag takes.
    pub fn as_flag(self) -> &'static str {
        match self {
            Self::CacheAffinity => "cache-affinity",
            Self::LeastLoaded => "least-loaded",
            Self::RoundRobin => "round-robin",
            Self::LowestLatency => "lowest-latency",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum ServiceType {
    #[default]
    ClusterIP,
    LoadBalancer,
    NodePort,
}

impl ServiceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClusterIP => "ClusterIP",
            Self::LoadBalancer => "LoadBalancer",
            Self::NodePort => "NodePort",
        }
    }
}

/// One deployment of fastllm-proxy: a control plane and a gateway.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "fastllm.io",
    version = "v1alpha1",
    kind = "FastllmProxy",
    plural = "fastllmproxies",
    shortname = "fllm",
    namespaced,
    status = "FastllmProxyStatus",
    printcolumn = r#"{"name":"Gateway","type":"string","jsonPath":".status.proxyReplicas"}"#,
    printcolumn = r#"{"name":"Control","type":"string","jsonPath":".status.controlReady"}"#,
    printcolumn = r#"{"name":"Image","type":"string","jsonPath":".spec.image"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FastllmProxySpec {
    /// The image **both** planes run.
    ///
    /// One field, not two, and that is the point: they share a database
    /// schema, so a proxy older than its control plane reads a snapshot with
    /// fields it does not understand. Making it impossible to express is
    /// cheaper than detecting it.
    #[serde(default = "default_image")]
    pub image: String,

    #[serde(default = "default_pull_policy")]
    pub image_pull_policy: String,

    /// Postgres connection string. Not a database this operator runs — see
    /// the module doc for why.
    pub database: SecretRef,

    /// The proxy token, shared by both planes.
    pub proxy_token: SecretRef,

    /// AES-256-GCM key for `model_backends.upstream_api_key` at rest.
    ///
    /// **Not regenerable.** Losing it loses every upstream credential in that
    /// database; changing it without running `reencrypt-backends` stops the
    /// control plane from starting.
    pub encryption_key: SecretRef,

    #[serde(default)]
    pub control: ControlSpec,

    #[serde(default)]
    pub proxy: ProxySpec,

    /// Contents of the `fastllm:` tuning block, verbatim, mounted as
    /// `config.yaml`. Absent means the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning: Option<String>,
}

/// What the controller last observed. Every field here is measured, not
/// echoed back from the spec — a status that repeats the spec tells an
/// operator nothing they did not already type.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FastllmProxyStatus {
    /// `ready/desired`, so a partially rolled-out gateway is visible in
    /// `kubectl get` without a second command.
    #[serde(default)]
    pub proxy_replicas: String,
    /// Whether the control plane has a ready replica.
    #[serde(default)]
    pub control_ready: bool,
    /// The generation this status describes. A status whose
    /// `observedGeneration` trails `metadata.generation` is a reconcile that
    /// has not landed yet, which is the difference between "converged" and
    /// "not looked at".
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub last_transition_time: String,
}

fn default_image() -> String {
    "ghcr.io/azrtydxb/fastllm-proxy:v0.1.0".to_string()
}
fn default_pull_policy() -> String {
    "IfNotPresent".to_string()
}
fn default_proxy_replicas() -> i32 {
    2
}
fn default_upstream_timeout() -> u32 {
    120
}
