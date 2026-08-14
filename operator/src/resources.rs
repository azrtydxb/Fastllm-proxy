//! The objects a `FastllmProxy` becomes.
//!
//! Every builder here is a pure function of the spec (plus, where it matters,
//! the config hash the controller resolved from the Secrets), which is what
//! makes the controller testable without a cluster: given a spec, the
//! Deployment it produces is a value you can assert against.

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::autoscaling::v2::{
    CrossVersionObjectReference, HorizontalPodAutoscaler, HorizontalPodAutoscalerSpec, MetricSpec,
    MetricTarget, ResourceMetricSource,
};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvVar,
    EnvVarSource, HTTPGetAction, LocalObjectReference, PodSecurityContext, PodSpec,
    PodTemplateSpec, Probe, SeccompProfile, SecretKeySelector, SecretVolumeSource, SecurityContext,
    Service, ServiceAccount, ServicePort, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec as K8sIngressSpec, IngressTLS, ServiceBackendPort,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::api::rbac::v1::{PolicyRule, Role, RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Resource;
use std::collections::BTreeMap;

use crate::crd::{FastllmProxy, PodOverrides, SecretRef};

pub const CONTROL: &str = "control";
pub const PROXY: &str = "proxy";
pub const BOOTSTRAP: &str = "bootstrap";

/// Annotation carrying the hash of everything a pod reads but does not mount
/// from a versioned object: the three Secrets, the TLS Secret, and the tuning
/// file.
///
/// This is what makes rotation work. `secretKeyRef` env is resolved once, at
/// pod start, so rewriting the Secret changes nothing until something
/// restarts the pod — a cert-manager renewal or a rotated proxy token would
/// otherwise sit there looking applied and doing nothing. Changing this
/// annotation changes the pod template, which is a rollout.
pub const CONFIG_HASH_ANNOTATION: &str = "fastllm.io/config-hash";

pub fn name_for(owner: &FastllmProxy, component: &str) -> String {
    format!(
        "{}-{}",
        owner.meta().name.as_deref().unwrap_or("fastllm"),
        component
    )
}

pub fn labels(owner: &FastllmProxy, component: &str) -> BTreeMap<String, String> {
    let instance = owner.meta().name.clone().unwrap_or_default();
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "fastllm-proxy".into()),
        ("app.kubernetes.io/instance".into(), instance),
        ("app.kubernetes.io/component".into(), component.to_string()),
        (
            "app.kubernetes.io/managed-by".into(),
            "fastllm-operator".into(),
        ),
    ])
}

/// Selector labels are a strict subset of [`labels`] and must never gain a
/// field that changes between releases: `spec.selector` is immutable on a
/// Deployment, so a version label in here would make every upgrade a delete
/// and recreate. Pod-template labels from `spec.*.pod.labels` stay out of it
/// for the same reason.
fn selector(owner: &FastllmProxy, component: &str) -> BTreeMap<String, String> {
    let instance = owner.meta().name.clone().unwrap_or_default();
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "fastllm-proxy".into()),
        ("app.kubernetes.io/instance".into(), instance),
        ("app.kubernetes.io/component".into(), component.to_string()),
    ])
}

/// Garbage collection is the API server's job, not the controller's: an owner
/// reference means deleting the `FastllmProxy` removes everything it made,
/// including after the operator itself is gone.
fn owner_ref(owner: &FastllmProxy) -> OwnerReference {
    OwnerReference {
        api_version: FastllmProxy::api_version(&()).to_string(),
        kind: FastllmProxy::kind(&()).to_string(),
        name: owner.meta().name.clone().unwrap_or_default(),
        uid: owner.meta().uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

fn meta(owner: &FastllmProxy, component: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name_for(owner, component)),
        namespace: owner.meta().namespace.clone(),
        labels: Some(labels(owner, component)),
        owner_references: Some(vec![owner_ref(owner)]),
        ..Default::default()
    }
}

fn meta_with_annotations(
    owner: &FastllmProxy,
    component: &str,
    annotations: &BTreeMap<String, String>,
) -> ObjectMeta {
    ObjectMeta {
        annotations: (!annotations.is_empty()).then(|| annotations.clone()),
        ..meta(owner, component)
    }
}

fn secret_env(name: &str, r: &SecretRef) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: r.name.clone(),
                key: r.key.clone(),
                optional: Some(false),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn plain_env(name: &str, value: impl Into<String>) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.into()),
        ..Default::default()
    }
}

fn probe(path: &str, port: &str, https: bool, period: i32, failures: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.to_string()),
            port: IntOrString::String(port.to_string()),
            scheme: Some(if https { "HTTPS".into() } else { "HTTP".into() }),
            ..Default::default()
        }),
        period_seconds: Some(period),
        failure_threshold: Some(failures),
        ..Default::default()
    }
}

/// Locked down the same way in both planes: non-root, no privilege
/// escalation, read-only root filesystem, every capability dropped.
fn hardened() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        capabilities: Some(k8s_openapi::api::core::v1::Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn pod_security(fs_group: Option<i64>) -> PodSecurityContext {
    PodSecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(65532),
        fs_group,
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn pull_secrets(owner: &FastllmProxy) -> Option<Vec<LocalObjectReference>> {
    let refs: Vec<_> = owner
        .spec
        .image_pull_secrets
        .iter()
        .map(|n| LocalObjectReference { name: n.clone() })
        .collect();
    (!refs.is_empty()).then_some(refs)
}

/// Pod template metadata: the controller's labels, then the operator's
/// requested ones, then the config hash — applied last so no override can
/// disable the rotation mechanism by colliding with it.
fn template_meta(
    owner: &FastllmProxy,
    component: &str,
    over: &PodOverrides,
    config_hash: &str,
) -> ObjectMeta {
    let mut l = labels(owner, component);
    l.extend(over.labels.clone());
    let mut a = over.annotations.clone();
    a.insert(CONFIG_HASH_ANNOTATION.to_string(), config_hash.to_string());
    ObjectMeta {
        labels: Some(l),
        annotations: Some(a),
        ..Default::default()
    }
}

/// The env both planes share.
fn common_env(owner: &FastllmProxy) -> Vec<EnvVar> {
    vec![plain_env(
        "FASTLLM_LOG",
        owner.spec.observability.log_level.clone(),
    )]
}

/// Flags every role takes, so logging and tracing are configured once rather
/// than twice with a chance of disagreeing.
fn observability_args(owner: &FastllmProxy) -> Vec<String> {
    let o = &owner.spec.observability;
    let mut args = vec![format!("--log-format={}", o.log_format.as_flag())];
    if let Some(endpoint) = &o.otlp_endpoint {
        args.push(format!("--otel-endpoint={endpoint}"));
        if let Some(n) = o.otlp_sample_one_in {
            args.push(format!("--otel-sample-one-in={n}"));
        }
    }
    args
}

fn scheduling(spec: &mut PodSpec, over: &PodOverrides) {
    spec.node_selector = (!over.node_selector.is_empty()).then(|| over.node_selector.clone());
    spec.tolerations = (!over.tolerations.is_empty()).then(|| over.tolerations.clone());
    spec.affinity = over.affinity.clone();
    spec.priority_class_name = over.priority_class_name.clone();
}

pub fn config_map(owner: &FastllmProxy) -> ConfigMap {
    ConfigMap {
        metadata: meta(owner, PROXY),
        data: Some(BTreeMap::from([(
            "config.yaml".to_string(),
            tuning_yaml(owner),
        )])),
        ..Default::default()
    }
}

/// The tuning file's contents, defaults included — a pure function, because
/// it is also an input to the config hash.
pub fn tuning_yaml(owner: &FastllmProxy) -> String {
    owner.spec.tuning.clone().unwrap_or_else(|| {
        "fastllm:\n  prefix_bytes: 2048\n  balance_abs: 8\n  balance_rel: 1.5\n  unhealthy_after: 2\n"
            .to_string()
    })
}

pub fn control_deployment(owner: &FastllmProxy, config_hash: &str) -> Deployment {
    let s = &owner.spec;
    let tls = s.control.tls_secret_name.as_ref();
    let over = &s.control.pod;

    let mut args = vec![
        "--role=control".to_string(),
        "--admin-port=4001".to_string(),
    ];
    args.extend(observability_args(owner));
    let mut mounts = Vec::new();
    let mut volumes = Vec::new();
    if let Some(secret) = tls {
        args.push("--tls-cert=/etc/fastllm/tls/tls.crt".into());
        args.push("--tls-key=/etc/fastllm/tls/tls.key".into());
        mounts.push(VolumeMount {
            name: "tls".into(),
            mount_path: "/etc/fastllm/tls".into(),
            read_only: Some(true),
            ..Default::default()
        });
        volumes.push(Volume {
            name: "tls".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret.clone()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    args.extend(over.extra_args.clone());

    let mut env = vec![
        secret_env("FASTLLM_DATABASE_URL", &s.database),
        secret_env("FASTLLM_PROXY_TOKEN", &s.proxy_token),
        secret_env("FASTLLM_ENCRYPTION_KEY", &s.encryption_key),
        // What makes the management UI's deployment screen appear, and the
        // only way the control plane learns it is operated at all. A Helm or
        // manifest install never sets this, so the screen is not merely
        // hidden there — the routes behind it 404. See src/control/k8s.rs.
        plain_env(
            "FASTLLM_OPERATOR_RESOURCE",
            format!(
                "{}/{}",
                owner.meta().namespace.clone().unwrap_or_default(),
                owner.meta().name.clone().unwrap_or_default()
            ),
        ),
    ];
    env.extend(common_env(owner));
    env.extend(over.extra_env.clone());

    let container = Container {
        name: "control".into(),
        image: Some(s.image.clone()),
        image_pull_policy: Some(s.image_pull_policy.clone()),
        args: Some(args),
        ports: Some(vec![ContainerPort {
            name: Some("admin".into()),
            container_port: 4001,
            ..Default::default()
        }]),
        env: Some(env),
        // Migrations run at startup against a database that may still be
        // coming up, so the startup budget is generous where liveness is not.
        readiness_probe: Some(probe("/healthz", "admin", tls.is_some(), 10, 3)),
        liveness_probe: Some(probe("/healthz", "admin", tls.is_some(), 20, 3)),
        startup_probe: Some(probe("/healthz", "admin", tls.is_some(), 3, 40)),
        resources: s.control.resources.clone(),
        security_context: Some(hardened()),
        volume_mounts: (!mounts.is_empty()).then_some(mounts),
        ..Default::default()
    };

    let mut pod = PodSpec {
        security_context: Some(pod_security(None)),
        containers: vec![container],
        volumes: (!volumes.is_empty()).then_some(volumes),
        image_pull_secrets: pull_secrets(owner),
        // Its own ServiceAccount, holding exactly `get` and `patch` on this
        // one FastllmProxy — see `control_role`.
        service_account_name: Some(name_for(owner, CONTROL)),
        ..Default::default()
    };
    scheduling(&mut pod, over);

    Deployment {
        metadata: meta(owner, CONTROL),
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            // Recreate, not RollingUpdate: two control planes briefly sharing
            // one database would both apply migrations at startup.
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".into()),
                ..Default::default()
            }),
            selector: LabelSelector {
                match_labels: Some(selector(owner, CONTROL)),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(template_meta(owner, CONTROL, over, config_hash)),
                spec: Some(pod),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The gateway.
///
/// `image` is passed in rather than read from the spec: during an upgrade the
/// controller holds the data plane at the image the *control plane* is
/// actually running until the control plane has rolled, so the two never
/// disagree across a schema change. See `main::reconcile`.
pub fn proxy_deployment(owner: &FastllmProxy, image: &str, config_hash: &str) -> Deployment {
    let s = &owner.spec;
    let tls = s.control.tls_secret_name.as_ref();
    let scheme = if tls.is_some() { "https" } else { "http" };
    let control_host = name_for(owner, CONTROL);
    let over = &s.proxy.pod;

    let mut args = vec![
        "--role=proxy".to_string(),
        format!("--policy={}", s.proxy.policy.as_flag()),
        format!("--upstream-timeout={}", s.proxy.upstream_timeout),
        "--health-interval=10".to_string(),
    ];
    args.extend(observability_args(owner));
    if let Some(w) = s.proxy.workers {
        args.push(format!("--workers={w}"));
    }
    if let Some(p) = s.proxy.pool_max_idle {
        args.push(format!("--pool-max-idle={p}"));
    }
    if let Some(m) = &s.proxy.classifier.model {
        args.push(format!("--classifier-model={m}"));
    }
    if let Some(m) = &s.proxy.classifier.tier2_model {
        args.push(format!("--classifier-tier2-model={m}"));
    }

    let mut mounts = vec![
        VolumeMount {
            name: "config".into(),
            mount_path: "/etc/fastllm".into(),
            read_only: Some(true),
            ..Default::default()
        },
        // The last-known-good snapshot. Without it a proxy restarting during
        // a control-plane outage comes up with nothing to serve, turning the
        // outage the fallback exists to survive into the one it prevents.
        VolumeMount {
            name: "snapshot-cache".into(),
            mount_path: "/var/lib/fastllm".into(),
            ..Default::default()
        },
    ];
    let mut volumes = vec![
        Volume {
            name: "config".into(),
            config_map: Some(ConfigMapVolumeSource {
                name: name_for(owner, PROXY),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: "snapshot-cache".into(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
    ];

    if let Some(secret) = tls {
        // Without this a privately-issued certificate fails the handshake and
        // the proxy quietly serves its cached snapshot for ever — which looks
        // like "configuration changes stopped working", not like a TLS error.
        args.push("--ca-bundle=/etc/fastllm/ca/ca.crt".into());
        mounts.push(VolumeMount {
            name: "control-ca".into(),
            mount_path: "/etc/fastllm/ca".into(),
            read_only: Some(true),
            ..Default::default()
        });
        volumes.push(Volume {
            name: "control-ca".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret.clone()),
                // ca.crt only: this pod has no use for tls.key and no
                // business holding it.
                items: Some(vec![k8s_openapi::api::core::v1::KeyToPath {
                    key: "ca.crt".into(),
                    path: "ca.crt".into(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    args.extend(over.extra_args.clone());

    let mut env = vec![
        plain_env(
            "FASTLLM_CONTROL_URL",
            format!("{scheme}://{control_host}:4001/snapshot"),
        ),
        secret_env("FASTLLM_PROXY_TOKEN", &s.proxy_token),
    ];
    env.extend(common_env(owner));
    env.extend(over.extra_env.clone());

    let container = Container {
        name: "proxy".into(),
        image: Some(image.to_string()),
        image_pull_policy: Some(s.image_pull_policy.clone()),
        args: Some(args),
        ports: Some(vec![ContainerPort {
            name: Some("http".into()),
            container_port: 4000,
            ..Default::default()
        }]),
        env: Some(env),
        // Readiness tracks backends — /health is 503 with none healthy, which
        // correctly pulls the pod out of the Service. Liveness must NOT use
        // it: a backend outage would then restart-loop a healthy proxy.
        readiness_probe: Some(probe("/health", "http", false, 10, 3)),
        liveness_probe: Some(probe("/metrics", "http", false, 20, 3)),
        startup_probe: Some(probe("/metrics", "http", false, 3, 20)),
        resources: s.proxy.resources.clone(),
        security_context: Some(hardened()),
        volume_mounts: Some(mounts),
        ..Default::default()
    };

    let mut pod = PodSpec {
        // emptyDir volumes are created root-owned by the kubelet, and
        // readOnlyRootFilesystem leaves nowhere else for the snapshot cache
        // to land.
        security_context: Some(pod_security(Some(65532))),
        containers: vec![container],
        volumes: Some(volumes),
        image_pull_secrets: pull_secrets(owner),
        // Above --shutdown-grace (25s), so in-flight generations finish
        // before the kubelet SIGKILLs.
        termination_grace_period_seconds: Some(40),
        topology_spread_constraints: Some(vec![
            k8s_openapi::api::core::v1::TopologySpreadConstraint {
                max_skew: 1,
                topology_key: "kubernetes.io/hostname".into(),
                // ScheduleAnyway, so a single-node cluster still runs both
                // replicas.
                when_unsatisfiable: "ScheduleAnyway".into(),
                label_selector: Some(LabelSelector {
                    match_labels: Some(selector(owner, PROXY)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    scheduling(&mut pod, over);

    Deployment {
        metadata: meta(owner, PROXY),
        spec: Some(DeploymentSpec {
            // Omitted entirely while an HPA owns it. Server-side apply only
            // enforces fields this manager sets, so *not writing* the field
            // is what lets the autoscaler keep it — writing it back every
            // reconcile would fight the HPA for ever.
            replicas: (!s.proxy.autoscaling.enabled).then_some(s.proxy.replicas),
            // maxUnavailable: 0 — a rollout only ever adds capacity.
            strategy: Some(DeploymentStrategy {
                type_: Some("RollingUpdate".into()),
                rolling_update: Some(k8s_openapi::api::apps::v1::RollingUpdateDeployment {
                    max_unavailable: Some(IntOrString::Int(0)),
                    max_surge: Some(IntOrString::Int(1)),
                }),
            }),
            selector: LabelSelector {
                match_labels: Some(selector(owner, PROXY)),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(template_meta(owner, PROXY, over, config_hash)),
                spec: Some(pod),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The one-shot Job that gives the management UI its first login.
///
/// `set-password` is idempotent — it resets the password of an existing
/// principal — but the controller still runs it exactly once, tracked by
/// `status.bootstrapped`: a Job that re-ran on every reconcile would reset an
/// operator's password back to whatever the Secret says, minutes after they
/// changed it.
pub fn bootstrap_job(owner: &FastllmProxy) -> Option<Job> {
    let b = owner.spec.bootstrap.as_ref()?;
    let container = Container {
        name: "set-password".into(),
        image: Some(owner.spec.image.clone()),
        image_pull_policy: Some(owner.spec.image_pull_policy.clone()),
        args: Some(vec![
            "set-password".to_string(),
            format!("--name={}", b.name),
        ]),
        env: Some(vec![
            secret_env("FASTLLM_DATABASE_URL", &owner.spec.database),
            secret_env("FASTLLM_BOOTSTRAP_PASSWORD", &b.password),
            plain_env("FASTLLM_LOG", owner.spec.observability.log_level.clone()),
        ]),
        security_context: Some(hardened()),
        ..Default::default()
    };
    let mut pod = PodSpec {
        restart_policy: Some("OnFailure".into()),
        security_context: Some(pod_security(None)),
        containers: vec![container],
        image_pull_secrets: pull_secrets(owner),
        ..Default::default()
    };
    // Scheduled like the control plane: it talks to the same database from
    // the same place, so a taint that keeps one off a node keeps both off it.
    scheduling(&mut pod, &owner.spec.control.pod);

    Some(Job {
        metadata: meta(owner, BOOTSTRAP),
        spec: Some(JobSpec {
            backoff_limit: Some(6),
            // Cleaned up by the API server once it has succeeded. The
            // controller does not need the object afterwards — it records the
            // outcome in `status.bootstrapped`, which survives.
            ttl_seconds_after_finished: Some(600),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels(owner, BOOTSTRAP)),
                    ..Default::default()
                }),
                spec: Some(pod),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

pub fn service(owner: &FastllmProxy, component: &str) -> Service {
    let (port, target, type_, annotations) = match component {
        CONTROL => (
            4001,
            "admin",
            // Always ClusterIP. This Service fronts /snapshot, which returns
            // decrypted upstream credentials to anything holding the proxy
            // token — so it is not a field on the spec.
            "ClusterIP",
            &owner.spec.control.service_annotations,
        ),
        _ => (
            4000,
            "http",
            owner.spec.proxy.service_type.as_str(),
            &owner.spec.proxy.service_annotations,
        ),
    };
    Service {
        metadata: meta_with_annotations(owner, component, annotations),
        spec: Some(ServiceSpec {
            type_: Some(type_.to_string()),
            selector: Some(selector(owner, component)),
            ports: Some(vec![ServicePort {
                name: Some(target.to_string()),
                port,
                target_port: Some(IntOrString::String(target.to_string())),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Keeps a rolling node drain from taking the whole gateway with it.
///
/// `None` below two replicas: a PDB of `minAvailable: 1` over a single pod
/// blocks every voluntary eviction, so a node drain hangs for ever rather
/// than protecting anything. With autoscaling on, the floor is `minReplicas`
/// for the same reason.
pub fn pod_disruption_budget(owner: &FastllmProxy) -> Option<PodDisruptionBudget> {
    let a = &owner.spec.proxy.autoscaling;
    let floor = if a.enabled {
        a.min_replicas
    } else {
        owner.spec.proxy.replicas
    };
    if floor < 2 {
        return None;
    }
    Some(PodDisruptionBudget {
        metadata: meta(owner, PROXY),
        spec: Some(PodDisruptionBudgetSpec {
            min_available: Some(IntOrString::Int(1)),
            selector: Some(LabelSelector {
                match_labels: Some(selector(owner, PROXY)),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

pub fn horizontal_pod_autoscaler(owner: &FastllmProxy) -> Option<HorizontalPodAutoscaler> {
    let a = &owner.spec.proxy.autoscaling;
    if !a.enabled {
        return None;
    }
    Some(HorizontalPodAutoscaler {
        metadata: meta(owner, PROXY),
        spec: Some(HorizontalPodAutoscalerSpec {
            scale_target_ref: CrossVersionObjectReference {
                api_version: Some("apps/v1".into()),
                kind: "Deployment".into(),
                name: name_for(owner, PROXY),
            },
            min_replicas: Some(a.min_replicas),
            max_replicas: a.max_replicas,
            metrics: Some(vec![MetricSpec {
                type_: "Resource".into(),
                resource: Some(ResourceMetricSource {
                    name: "cpu".into(),
                    target: MetricTarget {
                        type_: "Utilization".into(),
                        average_utilization: Some(a.target_cpu_utilization_percentage),
                        ..Default::default()
                    },
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    })
}

pub fn ingress(owner: &FastllmProxy) -> Option<Ingress> {
    let i = &owner.spec.proxy.ingress;
    if !i.enabled {
        return None;
    }
    let backend = IngressBackend {
        service: Some(IngressServiceBackend {
            name: name_for(owner, PROXY),
            port: Some(ServiceBackendPort {
                name: Some("http".into()),
                ..Default::default()
            }),
        }),
        ..Default::default()
    };
    Some(Ingress {
        metadata: meta_with_annotations(owner, PROXY, &i.annotations),
        spec: Some(K8sIngressSpec {
            ingress_class_name: i.class_name.clone(),
            rules: Some(vec![IngressRule {
                host: i.host.clone(),
                http: Some(HTTPIngressRuleValue {
                    paths: vec![HTTPIngressPath {
                        path: Some(i.path.clone()),
                        path_type: i.path_type.clone(),
                        backend,
                    }],
                }),
            }]),
            tls: i.tls_secret_name.as_ref().map(|s| {
                vec![IngressTLS {
                    hosts: i.host.clone().map(|h| vec![h]),
                    secret_name: Some(s.clone()),
                }]
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// A `ServiceMonitor`, built as plain JSON.
///
/// `monitoring.coreos.com` is not in `k8s-openapi`, and pulling in a crate for
/// one object with four fields would be a dependency for a struct literal.
/// The controller applies this through the dynamic API and treats a missing
/// CRD as "no Prometheus operator here", not as an error.
pub fn service_monitor(owner: &FastllmProxy) -> Option<serde_json::Value> {
    let m = &owner.spec.observability.service_monitor;
    if !m.enabled {
        return None;
    }
    let mut label_set = labels(owner, PROXY);
    label_set.extend(m.labels.clone());
    Some(serde_json::json!({
        "apiVersion": "monitoring.coreos.com/v1",
        "kind": "ServiceMonitor",
        "metadata": {
            "name": name_for(owner, PROXY),
            "namespace": owner.meta().namespace,
            "labels": label_set,
            "ownerReferences": [owner_ref(owner)],
        },
        "spec": {
            "selector": { "matchLabels": selector(owner, PROXY) },
            "endpoints": [{ "port": "http", "path": "/metrics", "interval": m.interval }],
        }
    }))
}

/// The control plane's own identity, so the UI can read and edit the resource
/// that describes this deployment.
///
/// A separate ServiceAccount rather than `default`: the token is mounted into
/// a pod that serves an admin API, and "what this pod may do to the cluster"
/// should be one object an operator can read, not an inherited default.
pub fn control_service_account(owner: &FastllmProxy) -> ServiceAccount {
    ServiceAccount {
        metadata: meta(owner, CONTROL),
        ..Default::default()
    }
}

/// One namespaced Role, naming one resource.
///
/// `resourceNames` is the point: this grants `get` and `patch` on *this*
/// `FastllmProxy` and no other, so a control plane cannot rewrite a different
/// deployment sharing its namespace. It gets no access to Secrets, pods,
/// Deployments or anything else — the UI edits a spec, and the operator is
/// what turns that into a rollout.
pub fn control_role(owner: &FastllmProxy) -> Role {
    Role {
        metadata: meta(owner, CONTROL),
        rules: Some(vec![PolicyRule {
            api_groups: Some(vec!["fastllm.io".into()]),
            resources: Some(vec!["fastllmproxies".into()]),
            resource_names: Some(vec![owner.meta().name.clone().unwrap_or_default()]),
            verbs: vec!["get".into(), "patch".into()],
            ..Default::default()
        }]),
    }
}

pub fn control_role_binding(owner: &FastllmProxy) -> RoleBinding {
    RoleBinding {
        metadata: meta(owner, CONTROL),
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".into(),
            kind: "Role".into(),
            name: name_for(owner, CONTROL),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".into(),
            name: name_for(owner, CONTROL),
            namespace: owner.meta().namespace.clone(),
            ..Default::default()
        }]),
    }
}
