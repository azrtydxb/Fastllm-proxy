//! The `FastllmProxy` custom resource.
//!
//! # Why a CRD rather than one more Helm chart
//!
//! The chart and the manifests both describe a deployment once, at apply
//! time. What they cannot do is *keep* describing it: a Deployment edited by
//! hand stays edited, a proxy pinned to an older image than its control plane
//! stays pinned, a rotated Secret never reaches a running pod, and nothing
//! notices until a snapshot arrives with fields the data plane does not
//! understand.
//!
//! This resource is the desired state, and the controller's job is to keep
//! the cluster equal to it. The properties worth having a controller for are
//! the ones a chart cannot enforce after the fact:
//!
//! - **The two planes always run the same image**, and the control plane
//!   reaches it *first*. They share a database schema, so an upgrade is
//!   ordered, not simultaneous — see `main.rs`.
//! - **A rotated Secret restarts what reads it.** The pod templates carry a
//!   hash of the resolved secret material, so rotating the proxy token or a
//!   renewed TLS certificate rolls the pods instead of silently doing
//!   nothing until something else happens to restart them.
//! - **Scaling means scaling the data plane.** The control plane is not
//!   exposed as a replica count, because a second control plane only races
//!   the first rebuilding snapshots.
//! - **The install finishes.** A deployment nobody can log into is not
//!   installed; `bootstrap` runs `set-password` as a Job once the control
//!   plane is ready.
//!
//! # Deliberately not modelled
//!
//! The database. A `FastllmProxy` points at a Secret holding a connection
//! string and never at a Postgres it operates: a database is a stateful
//! service with backup, failover and upgrade concerns that belong to whoever
//! runs it, and an operator that quietly owns one is an operator that can
//! quietly lose one.

use k8s_openapi::api::core::v1::{Affinity, EnvVar, ResourceRequirements, Toleration};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Where a value that must not be in git actually lives.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    /// Secret in the same namespace as this resource.
    pub name: String,
    /// Key within it.
    pub key: String,
}

/// Pod-level knobs both planes take.
///
/// One struct rather than two sets of fields: scheduling a control plane and
/// scheduling a gateway are the same problem, and a cluster that needs a
/// toleration for one usually needs it for the other. `extraArgs` and
/// `extraEnv` are the escape hatch for anything this CRD does not model yet —
/// without them, one unmodelled flag means abandoning the operator entirely.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PodOverrides {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<Toleration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<Affinity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_class_name: Option<String>,
    /// Merged into the pod template's annotations. The controller's own
    /// config-hash annotation is applied after these and wins on a collision.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    /// Merged into the pod template's labels, but never into the Deployment's
    /// selector — see `resources::selector`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Appended after every argument the controller computes, so a flag set
    /// here overrides the modelled one wherever `clap` takes the last value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_env: Vec<EnvVar>,
}

/// The first admin login.
///
/// Without this a successful reconcile leaves a UI nobody can sign into: a
/// freshly migrated database has no `password_hash` on any principal, and the
/// route that would set one is itself behind a session cookie. The controller
/// runs `fastllm-proxy set-password` as a Job once the control plane is ready
/// — the same trust boundary as holding cluster access, which whoever applied
/// this resource already has.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapSpec {
    /// Login name. Created if no principal by this name exists.
    #[serde(default = "default_admin_name")]
    pub name: String,
    /// Where the password lives. Never inline: this is the one credential
    /// that grants `config:write` over everything.
    pub password: SecretRef,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum LogFormat {
    #[default]
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json")]
    Json,
}

impl LogFormat {
    pub fn as_flag(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
        }
    }
}

/// A `ServiceMonitor` for the Prometheus operator.
///
/// Off by default and skipped without complaint when the CRD is absent: a
/// cluster with no Prometheus operator should not have its reconcile fail
/// over a resource kind that does not exist.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceMonitorSpec {
    #[serde(default)]
    pub enabled: bool,
    /// Scrape interval, Prometheus duration spelling.
    #[serde(default = "default_scrape_interval")]
    pub interval: String,
    /// Labels the Prometheus instance selects ServiceMonitors by. Without the
    /// one your Prometheus wants, the object exists and is never read.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilitySpec {
    /// `FASTLLM_LOG`, a `tracing` filter.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub log_format: LogFormat,
    /// OTLP/gRPC collector. Only does anything in an image built with the
    /// `otel` feature; a build without it ignores the flag rather than
    /// failing, so this is safe to set ahead of the image that uses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_endpoint: Option<String>,
    /// Head sampling: one trace in N. Unset means the binary's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_sample_one_in: Option<u32>,
    #[serde(default)]
    pub service_monitor: ServiceMonitorSpec,
}

// Every `Default` below is written out rather than derived, and they all call
// the same `default_*` functions serde does.
//
// `#[derive(Default)]` would fill these with `0`, `false` and `""` while
// applying a CR fills them from `#[serde(default = "...")]` — two different
// answers to "what does an unset field mean", and the derived one is the
// answer no user can ever produce. It is not hypothetical: it put a
// `minReplicas: 0` autoscaler in front of a test and would have rendered
// `FASTLLM_LOG=""` for anything constructing a spec in code.
// `spec_defaults_match_the_schema_defaults` (below) is what keeps them equal.

impl Default for ServiceMonitorSpec {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: default_scrape_interval(),
            labels: BTreeMap::new(),
        }
    }
}

impl Default for ObservabilitySpec {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            log_format: LogFormat::default(),
            otlp_endpoint: None,
            otlp_sample_one_in: None,
            service_monitor: ServiceMonitorSpec::default(),
        }
    }
}

impl Default for AutoscalingSpec {
    fn default() -> Self {
        Self {
            enabled: false,
            min_replicas: default_min_replicas(),
            max_replicas: default_max_replicas(),
            target_cpu_utilization_percentage: default_target_cpu(),
        }
    }
}

impl Default for IngressSpec {
    fn default() -> Self {
        Self {
            enabled: false,
            class_name: None,
            host: None,
            path: default_ingress_path(),
            path_type: default_path_type(),
            annotations: BTreeMap::new(),
            tls_secret_name: None,
        }
    }
}

/// The control plane. Its replica count is absent on purpose — see the type
/// doc on [`FastllmProxySpec`].
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
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
    /// `ca.crt` — a cert-manager `Certificate` produces exactly that. A
    /// renewal rolls both planes, because the certificate is part of the
    /// config hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_secret_name: Option<String>,

    /// Annotations on the admin Service — where a pinned load-balancer
    /// address goes when this plane is exposed at all.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub service_annotations: BTreeMap<String, String>,

    /// Service type for the admin listener.
    ///
    /// `ClusterIP` unless you mean otherwise, and the API server will refuse
    /// anything else without `tlsSecretName` — this Service fronts
    /// `/snapshot`, which hands *decrypted* upstream credentials to anything
    /// holding the proxy token, so exposing it in the clear is not a
    /// configuration, it is an accident. Exposed **and** TLS-only is a real
    /// deployment (see `deploy/README.md`, which runs exactly that on a
    /// pinned VIP), and the earlier design — hardcoded ClusterIP, no field —
    /// simply could not describe it.
    #[serde(default)]
    pub service_type: ServiceType,

    #[serde(default)]
    pub pod: PodOverrides,
}

/// Horizontal autoscaling for the gateway.
///
/// `replicas` is left alone while this is enabled — the HPA owns that field,
/// and two writers fighting over one number is a rollout that never settles.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AutoscalingSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_min_replicas")]
    pub min_replicas: i32,
    #[serde(default = "default_max_replicas")]
    pub max_replicas: i32,
    /// Target average CPU. Deliberately the only metric modelled: request
    /// concurrency is the number you actually want to scale on, and it needs
    /// a metrics adapter this operator has no business assuming.
    #[serde(default = "default_target_cpu")]
    pub target_cpu_utilization_percentage: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
    /// Host to route. Absent means a catch-all rule, which is rarely what
    /// anyone wants on a shared ingress controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default = "default_ingress_path")]
    pub path: String,
    #[serde(default = "default_path_type")]
    pub path_type: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    /// Secret holding the certificate the ingress terminates with. Only the
    /// gateway is ever exposed this way; the admin plane is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_secret_name: Option<String>,
}

/// One port on the gateway Service.
///
/// Every entry targets the container's HTTP port; this list is about which
/// addresses callers may use, not about the gateway listening more than once.
/// A deployment fronted at both `:80` and `:4000` — which is what happens the
/// moment anyone puts a plain `http://host/` in a client config — cannot be
/// described by a single hardcoded port, and losing the second one on
/// adoption is a silent outage for whoever was using it.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServicePortSpec {
    /// Port name. Kubernetes requires one on a multi-port Service, and it is
    /// the merge key an existing Service is patched against — keep it stable
    /// or the node port is reallocated.
    pub name: String,
    pub port: i32,
}

/// Semantic routing. Only meaningful in an image built with the `classifier`
/// features; the flags are inert otherwise.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierSpec {
    /// Tier-1 model directory inside the image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Tier-2 (contextual) model directory. Loaded lazily, and only when a
    /// rule names a refined class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier2_model: Option<String>,
}

/// The data plane: the part that scales.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProxySpec {
    /// Gateway replicas. Left to the HPA while `autoscaling.enabled` is true.
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

    /// Annotations on the gateway Service — where a load-balancer
    /// implementation takes its pinned address, its pool, or its health
    /// check. Without this a deployment on a real cluster cannot express the
    /// one thing it most needs to.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub service_annotations: BTreeMap<String, String>,

    /// Ports the gateway Service publishes. Empty means one port, 4000,
    /// named `http`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_ports: Vec<ServicePortSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,

    /// Worker threads. Unset means one per core, which is what the binary
    /// does on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<u32>,

    /// Idle upstream connections kept per backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_max_idle: Option<u32>,

    #[serde(default)]
    pub autoscaling: AutoscalingSpec,

    #[serde(default)]
    pub ingress: IngressSpec,

    #[serde(default)]
    pub classifier: ClassifierSpec,

    #[serde(default)]
    pub pod: PodOverrides,
}

impl Default for ProxySpec {
    fn default() -> Self {
        Self {
            replicas: default_proxy_replicas(),
            policy: Policy::default(),
            upstream_timeout: default_upstream_timeout(),
            service_type: ServiceType::default(),
            service_annotations: BTreeMap::new(),
            service_ports: Vec::new(),
            resources: None,
            workers: None,
            pool_max_idle: None,
            autoscaling: AutoscalingSpec::default(),
            ingress: IngressSpec::default(),
            classifier: ClassifierSpec::default(),
            pod: PodOverrides::default(),
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
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Gateway","type":"string","jsonPath":".status.proxyReplicas"}"#,
    printcolumn = r#"{"name":"Control","type":"string","jsonPath":".status.controlReady"}"#,
    printcolumn = r#"{"name":"Image","type":"string","jsonPath":".status.observedImage"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FastllmProxySpec {
    /// The image **both** planes run.
    ///
    /// One field, not two, and that is the point: they share a database
    /// schema, so a proxy older than its control plane reads a snapshot with
    /// fields it does not understand. Making it impossible to express is
    /// cheaper than detecting it, and the controller rolls the control plane
    /// to a new value before it touches the gateway.
    #[serde(default = "default_image")]
    pub image: String,

    #[serde(default = "default_pull_policy")]
    pub image_pull_policy: String,

    /// Names of `kubernetes.io/dockerconfigjson` Secrets, for a private
    /// registry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<String>,

    /// Postgres connection string. Not a database this operator runs — see
    /// the module doc for why.
    pub database: SecretRef,

    /// The proxy token, shared by both planes.
    pub proxy_token: SecretRef,

    /// AES-256-GCM key for `model_backends.upstream_api_key` at rest, as 64
    /// hex characters.
    ///
    /// **Immutable, and enforced by the API server** (see `crdgen`): losing
    /// it loses every upstream credential in that database, and changing it
    /// without running `reencrypt-backends` stops the control plane from
    /// starting. The CRD refuses the edit rather than letting a controller
    /// roll a deployment into that state.
    pub encryption_key: SecretRef,

    /// The first admin login. Absent means nobody can sign in until someone
    /// runs `set-password` by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<BootstrapSpec>,

    #[serde(default)]
    pub observability: ObservabilitySpec,

    #[serde(default)]
    pub control: ControlSpec,

    #[serde(default)]
    pub proxy: ProxySpec,

    /// Contents of the `fastllm:` tuning block, verbatim, mounted as
    /// `config.yaml`. Absent means the defaults. An edit here rolls the
    /// gateway, because the file is part of the config hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning: Option<String>,
}

/// What the controller last observed. Every field here is measured, not
/// echoed back from the spec — a status that repeats the spec tells an
/// operator nothing they did not already type.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FastllmProxyStatus {
    /// One word for `kubectl get`: `Pending`, `Upgrading`, `Bootstrapping`,
    /// `Degraded` or `Ready`. The conditions carry the detail.
    #[serde(default)]
    pub phase: String,
    /// `ready/desired`, so a partially rolled-out gateway is visible in
    /// `kubectl get` without a second command.
    #[serde(default)]
    pub proxy_replicas: String,
    /// Whether the control plane has a ready replica.
    #[serde(default)]
    pub control_ready: bool,
    /// The image actually serving traffic, which during an ordered upgrade is
    /// not yet `spec.image` — and never will be if the new one cannot be
    /// pulled. Printed instead of the spec for exactly that reason: the
    /// interesting number is what is running, not what was asked for.
    #[serde(default)]
    pub observed_image: String,
    /// Whether the bootstrap Job has completed. Never reset by the
    /// controller: re-running it is a password reset, and a controller that
    /// does that on its own is a controller that can lock an operator out.
    #[serde(default)]
    pub bootstrapped: bool,
    /// Hash of the resolved Secret material and tuning file the pods were
    /// last rendered with. Visible so "did the rotation take?" is one
    /// `kubectl get -o yaml` rather than a pod-template diff.
    #[serde(default)]
    pub config_hash: String,
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

impl Condition {
    /// Condition types this controller sets. `Ready` is the one an operator
    /// reads; the others say *why* it is not.
    pub const READY: &'static str = "Ready";
    pub const SECRETS_RESOLVED: &'static str = "SecretsResolved";
    pub const UPGRADING: &'static str = "Upgrading";
    pub const BOOTSTRAPPED: &'static str = "Bootstrapped";
}

/// The installable manifest: the derived schema plus the validation rules the
/// type system cannot carry.
///
/// Lives here rather than in `crdgen` so the drift test compares against the
/// same bytes the generator prints — a rule added to the binary alone would
/// be a rule the committed manifest never gained, which is exactly the drift
/// the test exists to catch.
pub fn manifest() -> serde_json::Value {
    use kube::CustomResourceExt;

    let mut crd = serde_json::to_value(FastllmProxy::crd()).expect("serialise CRD");
    let props = crd
        .pointer_mut("/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties")
        .and_then(|v| v.as_object_mut())
        .expect("generated CRD has spec properties");

    // CEL (`x-kubernetes-validations`) is enforced by the API server itself,
    // which is the only place it *can* be enforced: a controller that noticed
    // the edit afterwards would already be looking at a resource whose old
    // value is gone.
    //
    // Changing the encryption key without re-encrypting makes every stored
    // upstream credential undecryptable and stops the control plane from
    // starting. `reencrypt-backends` is the supported path, and it needs the
    // old key — which, by the time a controller could react, the resource no
    // longer names.
    props
        .get_mut("encryptionKey")
        .expect("encryptionKey is a required field")
        .as_object_mut()
        .expect("encryptionKey is an object schema")
        .insert(
            "x-kubernetes-validations".to_string(),
            serde_json::json!([{
                "rule": "self == oldSelf",
                "message": "encryptionKey is immutable: changing it makes every stored upstream \
                            credential undecryptable. Re-encrypt with `fastllm-proxy \
                            reencrypt-backends` first.",
            }]),
        );

    // Exposing the admin plane without TLS would put the session cookie, the
    // proxy token and every decrypted upstream credential on the network in
    // the clear. Refused at apply time rather than reported afterwards.
    if let Some(control) = props.get_mut("control").and_then(|v| v.as_object_mut()) {
        control.insert(
            "x-kubernetes-validations".to_string(),
            serde_json::json!([{
                "rule": "!has(self.serviceType) || self.serviceType == 'ClusterIP' || has(self.tlsSecretName)",
                "message": "control.serviceType other than ClusterIP requires control.tlsSecretName: \
                            the admin Service fronts /snapshot, which returns decrypted upstream \
                            credentials",
            }]),
        );
    }

    // A maximum below the minimum is an autoscaler that never scales, and the
    // API server can say so at apply time instead of an operator finding out
    // from a graph that never moves.
    if let Some(autoscaling) = props
        .get_mut("proxy")
        .and_then(|p| p.pointer_mut("/properties/autoscaling"))
        .and_then(|v| v.as_object_mut())
    {
        autoscaling.insert(
            "x-kubernetes-validations".to_string(),
            serde_json::json!([{
                "rule": "!self.enabled || self.maxReplicas >= self.minReplicas",
                "message": "autoscaling.maxReplicas must be >= minReplicas",
            }]),
        );
    }

    crd
}

/// The manifest as it is committed, header and all.
pub fn manifest_yaml() -> String {
    format!(
        "# Generated by `cargo run -p fastllm-operator --bin crdgen`. Do not edit.\n{}",
        serde_yaml::to_string(&manifest()).expect("serialise CRD")
    )
}

fn default_image() -> String {
    "ghcr.io/azrtydxb/fastllm-proxy:v0.2.0".to_string()
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
fn default_admin_name() -> String {
    "admin".to_string()
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_scrape_interval() -> String {
    "30s".to_string()
}
fn default_min_replicas() -> i32 {
    2
}
fn default_max_replicas() -> i32 {
    10
}
fn default_target_cpu() -> i32 {
    70
}
fn default_ingress_path() -> String {
    "/".to_string()
}
fn default_path_type() -> String {
    "Prefix".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two ways a field can be left unset must mean the same thing.
    ///
    /// serde fills an omitted field from `#[serde(default = "...")]`; Rust
    /// code that builds a spec fills it from `Default`. A derived `Default`
    /// makes the second one `0`/`false`/`""` — an autoscaler with a floor of
    /// zero replicas, an empty log filter — which is a configuration no
    /// applied YAML can produce and no reviewer would think to check.
    #[test]
    fn spec_defaults_match_the_schema_defaults() {
        let from_yaml: FastllmProxySpec = serde_yaml::from_str(
            "database: { name: db, key: uri }\n\
             proxyToken: { name: s, key: t }\n\
             encryptionKey: { name: s, key: k }\n",
        )
        .expect("a minimal spec");

        assert_eq!(from_yaml.proxy, ProxySpec::default());
        assert_eq!(from_yaml.observability, ObservabilitySpec::default());
        assert_eq!(from_yaml.control, ControlSpec::default());
        assert_eq!(from_yaml.proxy.autoscaling, AutoscalingSpec::default());
        assert_eq!(from_yaml.proxy.ingress, IngressSpec::default());
        assert_eq!(
            from_yaml.observability.service_monitor,
            ServiceMonitorSpec::default()
        );
        // And spot-check the two that would have been silently wrong.
        assert_eq!(AutoscalingSpec::default().min_replicas, 2);
        assert_eq!(ObservabilitySpec::default().log_level, "info");
    }

    /// A default that is not the image this release ships is an install that
    /// quietly deploys the previous one — the failure
    /// `tests/release_consistency.rs` exists to catch elsewhere.
    #[test]
    fn the_default_image_is_pinned_not_latest() {
        assert!(default_image().contains(":v"), "{}", default_image());
        assert!(!default_image().ends_with(":latest"));
    }
}
