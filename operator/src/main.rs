//! The controller loop.
//!
//! Server-side apply throughout, with a fixed field manager. That is what
//! makes reconciling repeatedly free: the API server diffs against the fields
//! this controller owns and leaves everything else — an annotation added by a
//! service mesh, a label added by a policy engine — untouched. A
//! read-modify-write loop would fight those tools for ever.

use fastllm_operator::{crd, resources};

use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, Client, Resource, ResourceExt};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

use crd::{Condition, FastllmProxy, FastllmProxyStatus};

/// Identifies this controller's field ownership in server-side apply.
/// Changing it orphans every field the previous name owned, so it is a
/// constant rather than something derived from a version.
const MANAGER: &str = "fastllm-operator";

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
    #[error("{0} has no namespace, which a namespaced resource always does")]
    NoNamespace(String),
}

struct Ctx {
    client: Client,
}

async fn apply<K>(api: &Api<K>, name: &str, obj: &K) -> Result<K, Error>
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug + serde::Serialize,
{
    Ok(api
        .patch(
            name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(obj),
        )
        .await?)
}

async fn reconcile(obj: Arc<FastllmProxy>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = obj
        .namespace()
        .ok_or_else(|| Error::NoNamespace(obj.name_any()))?;
    let name = obj.name_any();
    let client = &ctx.client;

    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), &ns);
    let svcs: Api<Service> = Api::namespaced(client.clone(), &ns);
    let pdbs: Api<PodDisruptionBudget> = Api::namespaced(client.clone(), &ns);

    let cm = resources::config_map(&obj);
    apply(&cms, &resources::name_for(&obj, resources::PROXY), &cm).await?;

    // Control plane before gateway. A proxy that starts first cannot reach a
    // control plane that does not exist yet, and would spend its first
    // interval falling back to a snapshot cache it has never written.
    let control = resources::control_deployment(&obj);
    apply(
        &deploys,
        &resources::name_for(&obj, resources::CONTROL),
        &control,
    )
    .await?;
    let control_svc = resources::service(&obj, resources::CONTROL);
    apply(
        &svcs,
        &resources::name_for(&obj, resources::CONTROL),
        &control_svc,
    )
    .await?;

    let proxy = resources::proxy_deployment(&obj);
    apply(
        &deploys,
        &resources::name_for(&obj, resources::PROXY),
        &proxy,
    )
    .await?;
    let proxy_svc = resources::service(&obj, resources::PROXY);
    apply(
        &svcs,
        &resources::name_for(&obj, resources::PROXY),
        &proxy_svc,
    )
    .await?;

    let pdb_name = resources::name_for(&obj, resources::PROXY);
    match resources::pod_disruption_budget(&obj) {
        Some(pdb) => {
            apply(&pdbs, &pdb_name, &pdb).await?;
        }
        // Scaling back to one replica has to remove the budget, not merely
        // stop creating it — a stale minAvailable:1 over a single pod blocks
        // every voluntary eviction and hangs a node drain.
        None => {
            if let Err(e) = pdbs.delete(&pdb_name, &Default::default()).await {
                if !matches!(&e, kube::Error::Api(r) if r.code == 404) {
                    return Err(e.into());
                }
            }
        }
    }

    // Status is measured from the cluster, not echoed from the spec.
    let control_ready = deploys
        .get_opt(&resources::name_for(&obj, resources::CONTROL))
        .await?
        .and_then(|d| d.status)
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0)
        > 0;
    let proxy_ready = deploys
        .get_opt(&resources::name_for(&obj, resources::PROXY))
        .await?
        .and_then(|d| d.status)
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);

    let ready = control_ready && proxy_ready >= 1;
    let condition_status = if ready { "True" } else { "False" };

    // Carry the previous timestamp forward while the condition has not
    // actually changed. Two reasons, and the second one is the bug this
    // fixes:
    //
    //   - `lastTransitionTime` is supposed to be the time the condition last
    //     *changed*. Stamping it every pass makes it the time of the last
    //     reconcile, which is a different and much less useful fact.
    //   - A status write is itself a change to the resource, so the watch
    //     fires and the controller reconciles again. With a fresh timestamp
    //     every pass the status is never equal to itself and the loop never
    //     settles — measured at roughly ten reconciles a second against an
    //     otherwise idle cluster.
    let previous = obj.status.as_ref();
    let last_transition_time = previous
        .and_then(|s| s.conditions.iter().find(|c| c.type_ == "Ready"))
        .filter(|c| c.status == condition_status)
        .map(|c| c.last_transition_time.clone())
        .unwrap_or_else(now_rfc3339);

    let status = FastllmProxyStatus {
        proxy_replicas: format!("{}/{}", proxy_ready, obj.spec.proxy.replicas),
        control_ready,
        observed_generation: obj.meta().generation.unwrap_or(0),
        conditions: vec![Condition {
            type_: "Ready".into(),
            status: condition_status.into(),
            reason: if ready {
                "AllReplicasReady"
            } else {
                "Progressing"
            }
            .into(),
            message: if control_ready {
                format!(
                    "{proxy_ready} of {} gateway replicas ready",
                    obj.spec.proxy.replicas
                )
            } else {
                "control plane has no ready replica".into()
            },
            last_transition_time,
        }],
    };

    // Write only on a real change. Even with a stable timestamp, an
    // unconditional patch bumps `metadata.resourceVersion` on every pass and
    // keeps the watch — and therefore this controller — busy for nothing.
    if previous != Some(&status) {
        let api: Api<FastllmProxy> = Api::namespaced(client.clone(), &ns);
        // A separate field manager for status: the spec and the status are
        // written by different code paths and must not share ownership.
        api.patch_status(
            &name,
            &PatchParams::apply("fastllm-operator-status").force(),
            &Patch::Apply(serde_json::json!({
                "apiVersion": FastllmProxy::api_version(&()),
                "kind": FastllmProxy::kind(&()),
                "status": status,
            })),
        )
        .await?;
    }

    // Re-reconcile on a timer as well as on events: readiness changes on the
    // Deployments this owns are watched, but a Secret being created later is
    // not, and that is the common "it came up eventually" case.
    Ok(Action::requeue(Duration::from_secs(300)))
}

fn now_rfc3339() -> String {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono_now())
        .0
        .to_rfc3339()
}

fn chrono_now() -> k8s_openapi::chrono::DateTime<k8s_openapi::chrono::Utc> {
    k8s_openapi::chrono::Utc::now()
}

fn on_error(obj: Arc<FastllmProxy>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    warn!(name = %obj.name_any(), error = %err, "reconcile failed, retrying");
    Action::requeue(Duration::from_secs(15))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls refuses to pick for you when more than one provider feature is
    // reachable in the dependency tree, and the failure is a panic at the
    // first handshake — which here is the first API call, so the operator
    // dies at startup with a message about crate features rather than
    // anything to do with Kubernetes. `ring` to match the proxy.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FASTLLM_OPERATOR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let client = Client::try_default().await?;
    let crs: Api<FastllmProxy> = Api::all(client.clone());

    // A missing CRD is the single most common way this fails to start, and
    // the default error is a bare 404 against a path nobody recognises.
    if let Err(e) = crs.list(&Default::default()).await {
        anyhow::bail!(
            "cannot list fastllmproxies.fastllm.io ({e}). Is the CRD installed? \
             `kubectl apply -f operator/deploy/crd.yaml`"
        );
    }

    info!("watching fastllmproxies.fastllm.io in all namespaces");
    Controller::new(crs, watcher::Config::default())
        .owns(
            Api::<Deployment>::all(client.clone()),
            watcher::Config::default(),
        )
        .owns(
            Api::<Service>::all(client.clone()),
            watcher::Config::default(),
        )
        .shutdown_on_signal()
        .run(reconcile, on_error, Arc::new(Ctx { client }))
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => info!(name = %obj.name, "reconciled"),
                Err(e) => error!(error = %e, "controller error"),
            }
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crd::{FastllmProxySpec, Policy, ProxySpec, SecretRef, ServiceType};

    fn spec() -> FastllmProxy {
        let mut cr = FastllmProxy::new(
            "demo",
            FastllmProxySpec {
                image: "ghcr.io/azrtydxb/fastllm-proxy:v0.2.0".into(),
                image_pull_policy: "IfNotPresent".into(),
                database: SecretRef {
                    name: "db".into(),
                    key: "uri".into(),
                },
                proxy_token: SecretRef {
                    name: "s".into(),
                    key: "proxy-token".into(),
                },
                encryption_key: SecretRef {
                    name: "s".into(),
                    key: "encryption-key".into(),
                },
                control: Default::default(),
                proxy: ProxySpec::default(),
                tuning: None,
            },
        );
        cr.meta_mut().namespace = Some("fastllm".into());
        cr.meta_mut().uid = Some("uid".into());
        cr
    }

    /// The property the whole CRD exists for: one `image` field reaches both
    /// Deployments, so they cannot be pinned apart.
    #[test]
    fn both_planes_run_the_same_image() {
        let cr = spec();
        let image_of = |d: &Deployment| {
            d.spec
                .as_ref()
                .unwrap()
                .template
                .spec
                .as_ref()
                .unwrap()
                .containers[0]
                .image
                .clone()
                .unwrap()
        };
        assert_eq!(
            image_of(&resources::control_deployment(&cr)),
            image_of(&resources::proxy_deployment(&cr))
        );
    }

    /// The admin Service fronts /snapshot, which hands decrypted upstream
    /// credentials to anything holding the proxy token. Making the gateway
    /// externally reachable must never carry the admin plane with it.
    #[test]
    fn exposing_the_gateway_leaves_the_admin_service_internal() {
        let mut cr = spec();
        cr.spec.proxy.service_type = ServiceType::LoadBalancer;
        let admin = resources::service(&cr, resources::CONTROL);
        let gateway = resources::service(&cr, resources::PROXY);
        assert_eq!(admin.spec.unwrap().type_.unwrap(), "ClusterIP");
        assert_eq!(gateway.spec.unwrap().type_.unwrap(), "LoadBalancer");
    }

    /// A PDB of minAvailable:1 over a single pod blocks every voluntary
    /// eviction, so a node drain hangs instead of draining.
    #[test]
    fn a_single_replica_gets_no_disruption_budget() {
        let mut cr = spec();
        cr.spec.proxy.replicas = 1;
        assert!(resources::pod_disruption_budget(&cr).is_none());
        cr.spec.proxy.replicas = 2;
        assert!(resources::pod_disruption_budget(&cr).is_some());
    }

    /// Selector labels are immutable on a Deployment. If one ever varies with
    /// the release, every upgrade becomes a delete and recreate.
    #[test]
    fn selector_labels_carry_nothing_that_changes_between_releases() {
        let cr = spec();
        let d = resources::proxy_deployment(&cr);
        let sel = d.spec.unwrap().selector.match_labels.unwrap();
        assert!(!sel.contains_key("app.kubernetes.io/version"));
        assert!(!sel.contains_key("app.kubernetes.io/managed-by"));
        assert_eq!(sel.get("app.kubernetes.io/component").unwrap(), "proxy");
    }

    /// Turning TLS on has to move the probes too — an HTTP probe against a
    /// TLS listener fails, and the control plane would never become ready.
    #[test]
    fn tls_moves_the_probes_to_https_as_well_as_the_listener() {
        let mut cr = spec();
        cr.spec.control.tls_secret_name = Some("fastllm-control-tls".into());
        let d = resources::control_deployment(&cr);
        let c = &d.spec.unwrap().template.spec.unwrap().containers[0];
        assert!(c
            .args
            .as_ref()
            .unwrap()
            .iter()
            .any(|a| a.starts_with("--tls-cert")));
        for p in [&c.readiness_probe, &c.liveness_probe, &c.startup_probe] {
            let scheme = p
                .as_ref()
                .unwrap()
                .http_get
                .as_ref()
                .unwrap()
                .scheme
                .clone();
            assert_eq!(scheme.unwrap(), "HTTPS");
        }
        // And the gateway must trust the issuing CA, or the handshake fails
        // and it serves a stale snapshot for ever.
        let p = resources::proxy_deployment(&cr);
        let pc = &p.spec.unwrap().template.spec.unwrap().containers[0];
        assert!(pc
            .args
            .as_ref()
            .unwrap()
            .iter()
            .any(|a| a.starts_with("--ca-bundle")));
        let url = pc
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "FASTLLM_CONTROL_URL")
            .unwrap();
        assert!(url.value.as_ref().unwrap().starts_with("https://"));
    }

    #[test]
    fn policy_reaches_the_flag_the_binary_accepts() {
        let mut cr = spec();
        cr.spec.proxy.policy = Policy::LowestLatency;
        let d = resources::proxy_deployment(&cr);
        let args = d.spec.unwrap().template.spec.unwrap().containers[0]
            .args
            .clone()
            .unwrap();
        assert!(args.contains(&"--policy=lowest-latency".to_string()));
    }

    /// Everything the operator makes has to be garbage-collected with the
    /// resource that asked for it, including after the operator is gone.
    #[test]
    fn every_object_is_owned_by_the_resource_that_asked_for_it() {
        let cr = spec();
        for meta in [
            resources::config_map(&cr).metadata,
            resources::control_deployment(&cr).metadata,
            resources::proxy_deployment(&cr).metadata,
            resources::service(&cr, resources::PROXY).metadata,
            resources::pod_disruption_budget(&cr).unwrap().metadata,
        ] {
            let owners = meta.owner_references.expect("owner reference");
            assert_eq!(owners[0].kind, "FastllmProxy");
            assert_eq!(owners[0].controller, Some(true));
        }
    }
}
