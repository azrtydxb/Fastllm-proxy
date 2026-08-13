//! The objects a `FastllmProxy` becomes.
//!
//! Every builder here is a pure function of the spec, which is what makes the
//! controller testable without a cluster: given a spec, the Deployment it
//! produces is a value you can assert against.

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvVar,
    EnvVarSource, HTTPGetAction, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    SeccompProfile, SecretKeySelector, SecretVolumeSource, SecurityContext, Service, ServicePort,
    ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Resource;
use std::collections::BTreeMap;

use crate::crd::{FastllmProxy, SecretRef};

pub const CONTROL: &str = "control";
pub const PROXY: &str = "proxy";

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
/// and recreate.
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

pub fn config_map(owner: &FastllmProxy) -> ConfigMap {
    let tuning = owner.spec.tuning.clone().unwrap_or_else(|| {
        "fastllm:\n  prefix_bytes: 2048\n  balance_abs: 8\n  balance_rel: 1.5\n  unhealthy_after: 2\n"
            .to_string()
    });
    ConfigMap {
        metadata: meta(owner, PROXY),
        data: Some(BTreeMap::from([("config.yaml".to_string(), tuning)])),
        ..Default::default()
    }
}

pub fn control_deployment(owner: &FastllmProxy) -> Deployment {
    let s = &owner.spec;
    let tls = s.control.tls_secret_name.as_ref();

    let mut args = vec![
        "--role=control".to_string(),
        "--admin-port=4001".to_string(),
    ];
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
        env: Some(vec![
            secret_env("FASTLLM_DATABASE_URL", &s.database),
            secret_env("FASTLLM_PROXY_TOKEN", &s.proxy_token),
            secret_env("FASTLLM_ENCRYPTION_KEY", &s.encryption_key),
            EnvVar {
                name: "FASTLLM_LOG".into(),
                value: Some("info".into()),
                ..Default::default()
            },
        ]),
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
                metadata: Some(ObjectMeta {
                    labels: Some(labels(owner, CONTROL)),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    security_context: Some(pod_security(None)),
                    containers: vec![container],
                    volumes: (!volumes.is_empty()).then_some(volumes),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn proxy_deployment(owner: &FastllmProxy) -> Deployment {
    let s = &owner.spec;
    let tls = s.control.tls_secret_name.as_ref();
    let scheme = if tls.is_some() { "https" } else { "http" };
    let control_host = name_for(owner, CONTROL);

    let mut args = vec![
        "--role=proxy".to_string(),
        format!("--policy={}", s.proxy.policy.as_flag()),
        format!("--upstream-timeout={}", s.proxy.upstream_timeout),
        "--health-interval=10".to_string(),
    ];

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

    let container = Container {
        name: "proxy".into(),
        image: Some(s.image.clone()),
        image_pull_policy: Some(s.image_pull_policy.clone()),
        args: Some(args),
        ports: Some(vec![ContainerPort {
            name: Some("http".into()),
            container_port: 4000,
            ..Default::default()
        }]),
        env: Some(vec![
            EnvVar {
                name: "FASTLLM_CONTROL_URL".into(),
                value: Some(format!("{scheme}://{control_host}:4001/snapshot")),
                ..Default::default()
            },
            secret_env("FASTLLM_PROXY_TOKEN", &s.proxy_token),
            EnvVar {
                name: "FASTLLM_LOG".into(),
                value: Some("info".into()),
                ..Default::default()
            },
        ]),
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

    Deployment {
        metadata: meta(owner, PROXY),
        spec: Some(DeploymentSpec {
            replicas: Some(s.proxy.replicas),
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
                metadata: Some(ObjectMeta {
                    labels: Some(labels(owner, PROXY)),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    // emptyDir volumes are created root-owned by the kubelet,
                    // and readOnlyRootFilesystem leaves nowhere else for the
                    // snapshot cache to land.
                    security_context: Some(pod_security(Some(65532))),
                    containers: vec![container],
                    volumes: Some(volumes),
                    // Above --shutdown-grace (25s), so in-flight generations
                    // finish before the kubelet SIGKILLs.
                    termination_grace_period_seconds: Some(40),
                    topology_spread_constraints: Some(vec![
                        k8s_openapi::api::core::v1::TopologySpreadConstraint {
                            max_skew: 1,
                            topology_key: "kubernetes.io/hostname".into(),
                            // ScheduleAnyway, so a single-node cluster still
                            // runs both replicas.
                            when_unsatisfiable: "ScheduleAnyway".into(),
                            label_selector: Some(LabelSelector {
                                match_labels: Some(selector(owner, PROXY)),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn service(owner: &FastllmProxy, component: &str) -> Service {
    let (port, target, type_) = match component {
        CONTROL => (
            4001,
            "admin",
            // Always ClusterIP. This Service fronts /snapshot, which returns
            // decrypted upstream credentials to anything holding the proxy
            // token — so it is not a field on the spec.
            "ClusterIP",
        ),
        _ => (4000, "http", owner.spec.proxy.service_type.as_str()),
    };
    Service {
        metadata: meta(owner, component),
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
/// `None` at one replica: a PDB of `minAvailable: 1` over a single pod blocks
/// every voluntary eviction, so a node drain hangs for ever rather than
/// protecting anything.
pub fn pod_disruption_budget(owner: &FastllmProxy) -> Option<PodDisruptionBudget> {
    if owner.spec.proxy.replicas < 2 {
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
