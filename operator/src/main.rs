//! The controller loop.
//!
//! Server-side apply throughout, with a fixed field manager. That is what
//! makes reconciling repeatedly free: the API server diffs against the fields
//! this controller owns and leaves everything else — an annotation added by a
//! service mesh, a label added by a policy engine — untouched. A
//! read-modify-write loop would fight those tools for ever.
//!
//! # The shape of one pass
//!
//! 1. **Preflight.** Resolve the Secrets. Anything missing or unusable stops
//!    the pass here with a condition that names it, rather than deploying
//!    pods that will fail to start for a reason only `kubectl describe` knows.
//! 2. **Control plane first, and *finished* first.** It owns the schema, so
//!    an upgrade rolls it before the gateway is allowed to move — see
//!    [`proxy_image`].
//! 3. **Everything else**, created or deleted to match the spec: the
//!    disruption budget, the autoscaler, the ingress, the ServiceMonitor.
//! 4. **Bootstrap**, once, so the install ends with a UI somebody can log
//!    into.
//! 5. **Status**, measured rather than echoed, and written only when it
//!    changed.

use fastllm_operator::{crd, lease, obs, preflight, resources};

use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service, ServiceAccount};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use kube::api::{DeleteParams, DynamicObject, Patch, PatchParams};
use kube::core::GroupVersionKind;
use kube::discovery::ApiResource;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use kube::runtime::watcher;
use kube::{Api, Client, Resource, ResourceExt};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

use crd::{Condition, FastllmProxy, FastllmProxyStatus};

/// Identifies this controller's field ownership in server-side apply.
/// Changing it orphans every field the previous name owned, so it is a
/// constant rather than something derived from a version.
const MANAGER: &str = "fastllm-operator";

/// Name of the Lease every replica competes for.
const LEASE: &str = "fastllm-operator-leader";

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
    #[error("{0} has no namespace, which a namespaced resource always does")]
    NoNamespace(String),
}

struct Ctx {
    client: Client,
    recorder: Recorder,
    metrics: Arc<obs::Metrics>,
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

/// Delete an object the spec no longer asks for.
///
/// A 404 is success: the desired state is "absent", and it is absent. Turning
/// an ingress off must not wedge the controller because somebody already
/// removed it by hand.
async fn prune<K>(api: &Api<K>, name: &str) -> Result<(), Error>
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match api.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(r)) if r.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Which image the gateway may run right now.
///
/// The control plane owns the database schema, so during an upgrade the two
/// planes must not cross: the gateway stays on whatever it is already running
/// until the control plane is fully rolled onto the target image and ready.
/// `None` means "do not create it yet" — on a first install there is nothing
/// to serve until a control plane exists, and a gateway that starts first
/// spends its life falling back to a snapshot cache it has never written.
///
/// This is the fix for the gap the original controller had: it applied both
/// Deployments in the same pass, so a new image reached the gateway and the
/// control plane simultaneously, and the skew this resource exists to prevent
/// was prevented only against hand edits — never against its own rollout.
fn proxy_image<'a>(
    target: &'a str,
    control_at_target: bool,
    currently_running: Option<&'a str>,
) -> Option<&'a str> {
    if control_at_target {
        return Some(target);
    }
    currently_running
}

/// Is this Deployment fully rolled onto `image` and serving?
///
/// All four conditions matter: the template has to name the image, the
/// Deployment controller has to have observed the generation that set it,
/// every replica has to be updated, and at least one has to be ready.
/// Checking only `readyReplicas` would call a half-finished rollout done.
fn deployment_at(image: &str, d: Option<&Deployment>) -> bool {
    let Some(d) = d else { return false };
    let spec_image = d
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.containers.first())
        .and_then(|c| c.image.as_deref());
    if spec_image != Some(image) {
        return false;
    }
    let generation = d.metadata.generation.unwrap_or_default();
    let Some(status) = d.status.as_ref() else {
        return false;
    };
    let desired = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    status.observed_generation.unwrap_or_default() >= generation
        && status.updated_replicas.unwrap_or(0) >= desired
        && status.ready_replicas.unwrap_or(0) >= 1
}

/// The image a live Deployment's pod template names, whatever its state.
fn running_image(d: Option<&Deployment>) -> Option<&str> {
    d?.spec
        .as_ref()?
        .template
        .spec
        .as_ref()?
        .containers
        .first()?
        .image
        .as_deref()
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
    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), &ns);
    let ingresses: Api<Ingress> = Api::namespaced(client.clone(), &ns);
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let sas: Api<ServiceAccount> = Api::namespaced(client.clone(), &ns);
    let roles: Api<Role> = Api::namespaced(client.clone(), &ns);
    let bindings: Api<RoleBinding> = Api::namespaced(client.clone(), &ns);

    let previous = obj.status.as_ref();
    let proxy_name = resources::name_for(&obj, resources::PROXY);
    let control_name = resources::name_for(&obj, resources::CONTROL);

    // ------------------------------------------------------------ 1
    let tuning = resources::tuning_yaml(&obj);
    let resolved = match preflight::resolve(&secrets, &obj.spec, &tuning).await {
        Ok(r) => r,
        Err(invalid) => {
            // Nothing is applied on this pass. Deploying pods that cannot
            // start would bury the real cause under a CrashLoopBackOff, and
            // an existing healthy deployment must not be rolled onto a
            // configuration already known to be broken.
            let message = invalid.to_string();
            warn!(name = %name, %message, "preflight failed; nothing applied");
            publish(
                &ctx,
                &obj,
                EventType::Warning,
                invalid.reason(),
                message.clone(),
            )
            .await;
            let status = FastllmProxyStatus {
                phase: "Degraded".into(),
                proxy_replicas: previous
                    .map(|p| p.proxy_replicas.clone())
                    .unwrap_or_default(),
                control_ready: previous.map(|p| p.control_ready).unwrap_or(false),
                observed_image: previous
                    .map(|p| p.observed_image.clone())
                    .unwrap_or_default(),
                bootstrapped: previous.map(|p| p.bootstrapped).unwrap_or(false),
                config_hash: previous.map(|p| p.config_hash.clone()).unwrap_or_default(),
                observed_generation: obj.meta().generation.unwrap_or(0),
                conditions: vec![
                    condition(
                        previous,
                        Condition::SECRETS_RESOLVED,
                        "False",
                        invalid.reason(),
                        &message,
                    ),
                    condition(
                        previous,
                        Condition::READY,
                        "False",
                        "PreflightFailed",
                        &message,
                    ),
                ],
            };
            write_status(client, &ns, &name, previous, status).await?;
            return Ok(Action::requeue(Duration::from_secs(30)));
        }
    };
    let hash = &resolved.config_hash;

    // ------------------------------------------------------------ 2
    apply(&cms, &proxy_name, &resources::config_map(&obj)).await?;
    // The control plane's identity, and its permission to read and patch the
    // one FastllmProxy it belongs to — what the management UI's deployment
    // screen runs on. Applied before the Deployment that mounts the token.
    apply(
        &sas,
        &control_name,
        &resources::control_service_account(&obj),
    )
    .await?;
    apply(&roles, &control_name, &resources::control_role(&obj)).await?;
    apply(
        &bindings,
        &control_name,
        &resources::control_role_binding(&obj),
    )
    .await?;
    apply(
        &deploys,
        &control_name,
        &resources::control_deployment(&obj, hash),
    )
    .await?;
    apply(
        &svcs,
        &control_name,
        &resources::service(&obj, resources::CONTROL),
    )
    .await?;

    let control_live = deploys.get_opt(&control_name).await?;
    let control_at_target = deployment_at(&obj.spec.image, control_live.as_ref());
    let control_ready = control_live
        .as_ref()
        .and_then(|d| d.status.as_ref())
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0)
        > 0;
    let proxy_live = deploys.get_opt(&proxy_name).await?;
    let image = proxy_image(
        &obj.spec.image,
        control_at_target,
        running_image(proxy_live.as_ref()),
    )
    .map(str::to_string);

    if let Some(image) = &image {
        if image != &obj.spec.image {
            info!(
                name = %name, holding = %image, target = %obj.spec.image,
                "gateway held at its current image until the control plane finishes rolling"
            );
        }
        apply(
            &deploys,
            &proxy_name,
            &resources::proxy_deployment(&obj, image, hash),
        )
        .await?;
        apply(
            &svcs,
            &proxy_name,
            &resources::service(&obj, resources::PROXY),
        )
        .await?;
    }

    // ------------------------------------------------------------ 3
    match resources::pod_disruption_budget(&obj) {
        // Scaling back to one replica has to remove the budget, not merely
        // stop creating it — a stale minAvailable:1 over a single pod blocks
        // every voluntary eviction and hangs a node drain.
        Some(pdb) => apply(&pdbs, &proxy_name, &pdb).await.map(|_| ())?,
        None => prune(&pdbs, &proxy_name).await?,
    }
    match resources::horizontal_pod_autoscaler(&obj) {
        Some(hpa) => apply(&hpas, &proxy_name, &hpa).await.map(|_| ())?,
        None => prune(&hpas, &proxy_name).await?,
    }
    match resources::ingress(&obj) {
        Some(ing) => apply(&ingresses, &proxy_name, &ing).await.map(|_| ())?,
        None => prune(&ingresses, &proxy_name).await?,
    }
    reconcile_service_monitor(&ctx, &obj, &ns, &proxy_name).await;

    // ------------------------------------------------------------ 4
    let mut bootstrapped = previous.map(|p| p.bootstrapped).unwrap_or(false);
    if !bootstrapped && control_ready {
        if let Some(job) = resources::bootstrap_job(&obj) {
            let job_name = resources::name_for(&obj, resources::BOOTSTRAP);
            let live = jobs.get_opt(&job_name).await?;
            let succeeded = live
                .as_ref()
                .and_then(|j| j.status.as_ref())
                .and_then(|s| s.succeeded)
                .unwrap_or(0)
                > 0;
            if succeeded {
                bootstrapped = true;
                info!(name = %name, "bootstrap complete; the admin login exists");
                publish(
                    &ctx,
                    &obj,
                    EventType::Normal,
                    "Bootstrapped",
                    "admin login created; the management UI can be signed into".to_string(),
                )
                .await;
            } else if live.is_none() {
                apply(&jobs, &job_name, &job).await?;
                publish(
                    &ctx,
                    &obj,
                    EventType::Normal,
                    "Bootstrapping",
                    "running set-password to create the first admin login".to_string(),
                )
                .await;
            }
        }
    }

    // ------------------------------------------------------------ 5
    let proxy_ready = proxy_live
        .as_ref()
        .and_then(|d| d.status.as_ref())
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);
    let desired_replicas = proxy_live
        .as_ref()
        .and_then(|d| d.spec.as_ref())
        .and_then(|s| s.replicas)
        .unwrap_or(obj.spec.proxy.replicas);
    let upgrading = !control_at_target || image.as_deref() != Some(obj.spec.image.as_str());
    let waiting_to_bootstrap = obj.spec.bootstrap.is_some() && !bootstrapped;
    let ready = control_ready && proxy_ready >= 1 && !upgrading;

    let phase = if !control_ready {
        "Pending"
    } else if upgrading {
        "Upgrading"
    } else if waiting_to_bootstrap {
        "Bootstrapping"
    } else if ready {
        "Ready"
    } else {
        "Degraded"
    };

    let mut conditions = vec![
        condition(
            previous,
            Condition::SECRETS_RESOLVED,
            "True",
            "Resolved",
            "every referenced Secret exists and is usable",
        ),
        condition(
            previous,
            Condition::READY,
            if ready { "True" } else { "False" },
            if ready {
                "AllReplicasReady"
            } else {
                "Progressing"
            },
            &ready_message(control_ready, proxy_ready, desired_replicas),
        ),
    ];
    if upgrading {
        conditions.push(condition(
            previous,
            Condition::UPGRADING,
            "True",
            "RollingControlPlane",
            &format!(
                "control plane moving to {}; gateway held at {}",
                obj.spec.image,
                image.as_deref().unwrap_or("(not yet created)")
            ),
        ));
    }
    if obj.spec.bootstrap.is_some() {
        conditions.push(condition(
            previous,
            Condition::BOOTSTRAPPED,
            if bootstrapped { "True" } else { "False" },
            if bootstrapped { "Complete" } else { "Pending" },
            if bootstrapped {
                "an admin login exists"
            } else {
                "waiting for the set-password Job"
            },
        ));
    }

    // What is actually serving traffic, which during an upgrade is not
    // `spec.image` — and, if the new image cannot be pulled, never will be.
    // Reporting the control Deployment's template here instead would print
    // the version nothing is running, which is the opposite of the question
    // `kubectl get` is being asked. Observed against a deliberately
    // unpullable tag: the column claimed the new version while the gateway
    // served the old one.
    let observed_image = image
        .clone()
        .or_else(|| running_image(control_live.as_ref()).map(str::to_string))
        .unwrap_or_else(|| obj.spec.image.clone());

    let status = FastllmProxyStatus {
        phase: phase.to_string(),
        proxy_replicas: format!("{proxy_ready}/{desired_replicas}"),
        control_ready,
        observed_image,
        bootstrapped,
        config_hash: hash.clone(),
        observed_generation: obj.meta().generation.unwrap_or(0),
        conditions,
    };
    write_status(client, &ns, &name, previous, status).await?;

    ctx.metrics.reconciles.fetch_add(1, Ordering::Relaxed);

    // Re-reconcile on a timer as well as on events: readiness changes on the
    // Deployments this owns are watched, but a Secret being created later is
    // not, and that is the common "it came up eventually" case. Mid-rollout
    // and mid-bootstrap the answer changes on its own, so look sooner.
    Ok(Action::requeue(if ready {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(15)
    }))
}

/// Apply or remove the `ServiceMonitor`, tolerating a cluster with no
/// Prometheus operator.
///
/// A missing `monitoring.coreos.com` CRD is a 404 on the resource *kind*, and
/// it must not fail the pass: the deployment is fine, the cluster simply has
/// nowhere to register a scrape target.
async fn reconcile_service_monitor(ctx: &Ctx, obj: &FastllmProxy, ns: &str, name: &str) {
    let gvk = GroupVersionKind::gvk("monitoring.coreos.com", "v1", "ServiceMonitor");
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::namespaced_with(ctx.client.clone(), ns, &ar);

    let outcome = match resources::service_monitor(obj) {
        Some(sm) => match serde_json::from_value::<DynamicObject>(sm) {
            Ok(o) => api
                .patch(
                    name,
                    &PatchParams::apply(MANAGER).force(),
                    &Patch::Apply(&o),
                )
                .await
                .map(|_| ()),
            Err(e) => {
                warn!(error = %e, "could not build a ServiceMonitor");
                return;
            }
        },
        None => match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(r)) if r.code == 404 => Ok(()),
            Err(e) => Err(e),
        },
    };
    match outcome {
        Ok(()) => {}
        Err(kube::Error::Api(r)) if r.code == 404 => {
            warn!("a ServiceMonitor was asked for but monitoring.coreos.com/v1 is not installed")
        }
        Err(e) => warn!(error = %e, "ServiceMonitor could not be reconciled; continuing"),
    }
}

/// What the `Ready` condition says when it is not ready.
///
/// The zero-replicas case is spelled out because it is the one a first
/// install hits and the one that reads as a broken deployment when it is not:
/// a gateway's readiness tracks its *backends*, so it stays unready until a
/// model exists. Observed on a real first install, sitting in `Degraded` with
/// nothing wrong with it.
fn ready_message(control_ready: bool, proxy_ready: i32, desired: i32) -> String {
    if !control_ready {
        "control plane has no ready replica".to_string()
    } else if proxy_ready == 0 {
        format!(
            "0 of {desired} gateway replicas ready; a gateway reports unready until at least \
             one model backend is healthy, so a new install stays here until a model is added \
             through the admin API or the UI"
        )
    } else {
        format!("{proxy_ready} of {desired} gateway replicas ready")
    }
}

/// One condition, carrying its previous `lastTransitionTime` forward while
/// the status has not changed.
///
/// Two reasons, and the second one is a bug this already had once:
///
///   - `lastTransitionTime` is supposed to be the time the condition last
///     *changed*. Stamping it every pass makes it the time of the last
///     reconcile, which is a different and much less useful fact.
///   - A status write is itself a change to the resource, so the watch fires
///     and the controller reconciles again. With a fresh timestamp every pass
///     the status is never equal to itself and the loop never settles —
///     measured at roughly ten reconciles a second against an otherwise idle
///     cluster.
fn condition(
    previous: Option<&FastllmProxyStatus>,
    type_: &str,
    status: &str,
    reason: &str,
    message: &str,
) -> Condition {
    let last_transition_time = previous
        .and_then(|s| s.conditions.iter().find(|c| c.type_ == type_))
        .filter(|c| c.status == status)
        .map(|c| c.last_transition_time.clone())
        .unwrap_or_else(now_rfc3339);
    Condition {
        type_: type_.to_string(),
        status: status.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time,
    }
}

/// Write the status, but only on a real change. Even with stable timestamps,
/// an unconditional patch bumps `metadata.resourceVersion` on every pass and
/// keeps the watch — and therefore this controller — busy for nothing.
async fn write_status(
    client: &Client,
    ns: &str,
    name: &str,
    previous: Option<&FastllmProxyStatus>,
    status: FastllmProxyStatus,
) -> Result<(), Error> {
    if previous == Some(&status) {
        return Ok(());
    }
    let api: Api<FastllmProxy> = Api::namespaced(client.clone(), ns);
    // A separate field manager for status: the spec and the status are
    // written by different code paths and must not share ownership.
    api.patch_status(
        name,
        &PatchParams::apply("fastllm-operator-status").force(),
        &Patch::Apply(serde_json::json!({
            "apiVersion": FastllmProxy::api_version(&()),
            "kind": FastllmProxy::kind(&()),
            "status": status,
        })),
    )
    .await?;
    Ok(())
}

/// Emit a Kubernetes Event.
///
/// Conditions say what is true now; Events say what happened, and they are
/// what `kubectl describe` shows an operator looking at this for the first
/// time. Failing to record one is never a reason to fail the pass.
async fn publish(ctx: &Ctx, obj: &FastllmProxy, type_: EventType, reason: &str, note: String) {
    let event = Event {
        type_,
        reason: reason.to_string(),
        note: Some(note),
        action: "Reconcile".into(),
        secondary: None,
    };
    if let Err(e) = ctx.recorder.publish(&event, &obj.object_ref(&())).await {
        warn!(error = %e, "could not record an event");
    }
}

fn now_rfc3339() -> String {
    k8s_openapi::chrono::Utc::now().to_rfc3339()
}

fn on_error(obj: Arc<FastllmProxy>, err: &Error, ctx: Arc<Ctx>) -> Action {
    ctx.metrics.errors.fetch_add(1, Ordering::Relaxed);
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

    let metrics = Arc::new(obs::Metrics::default());
    obs::spawn(Arc::clone(&metrics), ([0, 0, 0, 0], 8080).into());

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
    metrics.started.store(true, Ordering::Relaxed);

    // Every replica runs; one reconciles. See `lease` for why this is not
    // solved by pinning the Deployment to a single replica.
    let identity = std::env::var("POD_NAME")
        // Only reachable outside a cluster, where there is one process and
        // nothing to contend with.
        .unwrap_or_else(|_| format!("local-{}", std::process::id()));
    let lease_ns = std::env::var("POD_NAMESPACE").unwrap_or_else(|_| "fastllm-system".into());
    let elector = lease::LeaderElector::new(
        Api::<Lease>::namespaced(client.clone(), &lease_ns),
        LEASE,
        identity.clone(),
    );
    info!(%identity, namespace = %lease_ns, "waiting for leadership");
    elector.acquire().await?;
    metrics.leader.store(true, Ordering::Relaxed);

    // Losing the lease means another replica now owns these objects. Two
    // writers is exactly what the lease exists to prevent, so this replica
    // stops rather than carrying on with a claim it no longer holds; the
    // container restarts and queues up behind the new leader.
    let watchdog = {
        let metrics = Arc::clone(&metrics);
        tokio::spawn(async move {
            let lost = elector.keep().await;
            metrics.leader.store(false, Ordering::Relaxed);
            error!(reason = %lost, "leadership lost; exiting so the standby can take over");
            std::process::exit(1);
        })
    };

    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: MANAGER.into(),
            instance: Some(identity),
        },
    );

    info!("watching fastllmproxies.fastllm.io in all namespaces");
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        recorder,
        metrics: Arc::clone(&metrics),
    });
    Controller::new(crs, watcher::Config::default())
        .owns(
            Api::<Deployment>::all(client.clone()),
            watcher::Config::default(),
        )
        .owns(
            Api::<Service>::all(client.clone()),
            watcher::Config::default(),
        )
        .owns(Api::<Job>::all(client.clone()), watcher::Config::default())
        .shutdown_on_signal()
        .run(reconcile, on_error, ctx)
        .for_each(|res| {
            let metrics = Arc::clone(&metrics);
            async move {
                match res {
                    Ok((obj, _)) => {
                        metrics.resources.store(1, Ordering::Relaxed);
                        info!(name = %obj.name, "reconciled");
                    }
                    Err(e) => error!(error = %e, "controller error"),
                }
            }
        })
        .await;
    watchdog.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crd::{
        AutoscalingSpec, BootstrapSpec, FastllmProxySpec, IngressSpec, Policy, ProxySpec,
        SecretRef, ServiceType,
    };
    use k8s_openapi::api::apps::v1::DeploymentStatus;

    fn spec() -> FastllmProxy {
        let mut cr = FastllmProxy::new(
            "demo",
            FastllmProxySpec {
                image: "ghcr.io/azrtydxb/fastllm-proxy:v0.2.0".into(),
                image_pull_policy: "IfNotPresent".into(),
                image_pull_secrets: Vec::new(),
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
                bootstrap: None,
                observability: Default::default(),
                control: Default::default(),
                proxy: ProxySpec::default(),
                tuning: None,
            },
        );
        cr.meta_mut().namespace = Some("fastllm".into());
        cr.meta_mut().uid = Some("uid".into());
        cr
    }

    fn rolled(image: &str, replicas: i32) -> Deployment {
        let mut d = resources::control_deployment(&spec(), "hash");
        d.spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap()
            .containers[0]
            .image = Some(image.into());
        d.spec.as_mut().unwrap().replicas = Some(replicas);
        d.metadata.generation = Some(4);
        d.status = Some(DeploymentStatus {
            observed_generation: Some(4),
            updated_replicas: Some(replicas),
            ready_replicas: Some(replicas),
            ..Default::default()
        });
        d
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
            image_of(&resources::control_deployment(&cr, "h")),
            image_of(&resources::proxy_deployment(&cr, &cr.spec.image, "h"))
        );
    }

    /// The upgrade the original controller got wrong: a new image must not
    /// reach the gateway until the control plane is *finished* rolling onto
    /// it, because the two share a database schema.
    #[test]
    fn the_gateway_is_held_at_its_old_image_until_the_control_plane_has_rolled() {
        let old = "ghcr.io/azrtydxb/fastllm-proxy:v0.1.0";
        let new = "ghcr.io/azrtydxb/fastllm-proxy:v0.2.0";
        assert_eq!(proxy_image(new, false, Some(old)), Some(old), "held");
        assert_eq!(proxy_image(new, true, Some(old)), Some(new), "released");
    }

    /// On a first install there is no gateway to hold, and starting one
    /// before a control plane exists means a proxy whose first act is to fail
    /// to fetch a snapshot it has never cached.
    #[test]
    fn no_gateway_is_created_before_a_control_plane_is_ready() {
        assert_eq!(proxy_image("img", false, None), None);
        assert_eq!(proxy_image("img", true, None), Some("img"));
    }

    /// "Ready" is not "one pod answered". A rollout that has replaced two of
    /// three replicas is still a rollout, and releasing the gateway then
    /// would put a new data plane against a half-old control plane.
    #[test]
    fn a_half_finished_rollout_does_not_count_as_rolled() {
        let image = "img:2";
        let mut half = rolled(image, 3);
        half.status.as_mut().unwrap().updated_replicas = Some(2);
        assert!(!deployment_at(image, Some(&half)));

        // Nor does a spec the Deployment controller has not looked at yet.
        let mut stale = rolled(image, 1);
        stale.status.as_mut().unwrap().observed_generation = Some(3);
        assert!(!deployment_at(image, Some(&stale)));

        assert!(deployment_at(image, Some(&rolled(image, 3))));
        assert!(!deployment_at("img:3", Some(&rolled(image, 3))));
        assert!(!deployment_at(image, None));
    }

    /// Rotation is the whole reason the hash exists: same spec, different
    /// secret material, must be a different pod template.
    #[test]
    fn a_new_config_hash_changes_the_pod_template() {
        let cr = spec();
        let annotation = |d: &Deployment| {
            d.spec
                .as_ref()
                .unwrap()
                .template
                .metadata
                .as_ref()
                .unwrap()
                .annotations
                .clone()
                .unwrap()[resources::CONFIG_HASH_ANNOTATION]
                .clone()
        };
        assert_ne!(
            annotation(&resources::control_deployment(&cr, "aaaa")),
            annotation(&resources::control_deployment(&cr, "bbbb"))
        );
        // And the gateway rolls too — a rotated proxy token is read by both.
        assert_ne!(
            annotation(&resources::proxy_deployment(&cr, "img", "aaaa")),
            annotation(&resources::proxy_deployment(&cr, "img", "bbbb"))
        );
    }

    /// An operator's own annotation must not be able to switch rotation off
    /// by colliding with the controller's key.
    #[test]
    fn a_pod_annotation_cannot_overwrite_the_config_hash() {
        let mut cr = spec();
        cr.spec.proxy.pod.annotations.insert(
            resources::CONFIG_HASH_ANNOTATION.to_string(),
            "supplied-by-hand".into(),
        );
        let d = resources::proxy_deployment(&cr, "img", "real-hash");
        let a = d
            .spec
            .unwrap()
            .template
            .metadata
            .unwrap()
            .annotations
            .unwrap();
        assert_eq!(a[resources::CONFIG_HASH_ANNOTATION], "real-hash");
    }

    /// The admin Service fronts /snapshot, which hands decrypted upstream
    /// credentials to anything holding the proxy token. Making the gateway
    /// externally reachable must never carry the admin plane with it — they
    /// are separate fields, and exposing the admin one is refused by the CRD
    /// without TLS (see `manifest`'s CEL rules).
    #[test]
    fn exposing_the_gateway_leaves_the_admin_service_internal() {
        let mut cr = spec();
        cr.spec.proxy.service_type = ServiceType::LoadBalancer;
        let admin = resources::service(&cr, resources::CONTROL);
        let gateway = resources::service(&cr, resources::PROXY);
        assert_eq!(admin.spec.unwrap().type_.unwrap(), "ClusterIP");
        assert_eq!(gateway.spec.unwrap().type_.unwrap(), "LoadBalancer");

        // And when it is deliberately exposed, with TLS, it is that.
        cr.spec.control.service_type = ServiceType::LoadBalancer;
        cr.spec.control.tls_secret_name = Some("fastllm-control-tls".into());
        assert_eq!(
            resources::service(&cr, resources::CONTROL)
                .spec
                .unwrap()
                .type_
                .unwrap(),
            "LoadBalancer"
        );
    }

    /// The knob a real cluster cannot do without: a pinned load-balancer
    /// address lives in an annotation, and without this the operator could
    /// not express it at all.
    #[test]
    fn service_annotations_reach_the_service_that_asked_for_them() {
        let mut cr = spec();
        cr.spec
            .proxy
            .service_annotations
            .insert("io.cilium/lb-ipam-ips".into(), "192.168.10.125".into());
        let svc = resources::service(&cr, resources::PROXY);
        assert_eq!(
            svc.metadata.annotations.unwrap()["io.cilium/lb-ipam-ips"],
            "192.168.10.125"
        );
        // And not onto the admin Service, which shares the builder.
        assert!(resources::service(&cr, resources::CONTROL)
            .metadata
            .annotations
            .is_none());
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

        // Under an autoscaler the floor is minReplicas, not the replicas
        // field the HPA is ignoring.
        cr.spec.proxy.replicas = 5;
        cr.spec.proxy.autoscaling = AutoscalingSpec {
            enabled: true,
            min_replicas: 1,
            max_replicas: 9,
            target_cpu_utilization_percentage: 70,
        };
        assert!(resources::pod_disruption_budget(&cr).is_none());
    }

    /// Writing `replicas` back on every pass would fight the autoscaler for
    /// ever. Server-side apply only enforces fields this manager sets, so the
    /// fix is to leave the field out entirely.
    #[test]
    fn an_autoscaled_gateway_has_its_replica_count_left_alone() {
        let mut cr = spec();
        cr.spec.proxy.autoscaling.enabled = true;
        let d = resources::proxy_deployment(&cr, "img", "h");
        assert!(d.spec.unwrap().replicas.is_none());

        let hpa = resources::horizontal_pod_autoscaler(&cr).expect("hpa");
        assert_eq!(hpa.spec.unwrap().scale_target_ref.name, "demo-proxy");
    }

    #[test]
    fn an_ingress_exists_only_when_asked_for() {
        let mut cr = spec();
        assert!(resources::ingress(&cr).is_none());
        cr.spec.proxy.ingress = IngressSpec {
            enabled: true,
            host: Some("llm.example.com".into()),
            tls_secret_name: Some("llm-tls".into()),
            ..Default::default()
        };
        let ing = resources::ingress(&cr).expect("ingress");
        let ing = ing.spec.unwrap();
        assert_eq!(
            ing.rules.as_ref().unwrap()[0].host.as_deref(),
            Some("llm.example.com")
        );
        assert_eq!(ing.tls.unwrap()[0].secret_name.as_deref(), Some("llm-tls"));
    }

    /// The install is not finished until somebody can log in.
    #[test]
    fn the_bootstrap_job_runs_set_password_with_the_password_from_a_secret() {
        let mut cr = spec();
        assert!(resources::bootstrap_job(&cr).is_none(), "absent by default");
        cr.spec.bootstrap = Some(BootstrapSpec {
            name: "admin".into(),
            password: SecretRef {
                name: "s".into(),
                key: "admin-password".into(),
            },
        });
        let job = resources::bootstrap_job(&cr).expect("job");
        let c = &job.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(
            c.args.as_ref().unwrap(),
            &vec!["set-password".to_string(), "--name=admin".to_string()]
        );
        let password = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "FASTLLM_BOOTSTRAP_PASSWORD")
            .expect("password env");
        // From a Secret, never a literal — a Job spec is readable by anything
        // that can list Jobs in the namespace.
        assert!(password.value.is_none());
        assert_eq!(
            password
                .value_from
                .as_ref()
                .unwrap()
                .secret_key_ref
                .as_ref()
                .unwrap()
                .key,
            "admin-password"
        );
    }

    /// Selector labels are immutable on a Deployment. If one ever varies with
    /// the release, every upgrade becomes a delete and recreate — and a
    /// user-supplied pod label must not leak into it either.
    #[test]
    fn selector_labels_carry_nothing_that_changes_between_releases() {
        let mut cr = spec();
        cr.spec.proxy.pod.labels.insert("team".into(), "ml".into());
        let d = resources::proxy_deployment(&cr, "img", "h").spec.unwrap();
        let sel = d.selector.match_labels.unwrap();
        assert!(!sel.contains_key("app.kubernetes.io/version"));
        assert!(!sel.contains_key("app.kubernetes.io/managed-by"));
        assert!(!sel.contains_key("team"));
        assert_eq!(sel.get("app.kubernetes.io/component").unwrap(), "proxy");
        // But it does reach the pods, which is what it was for.
        assert_eq!(d.template.metadata.unwrap().labels.unwrap()["team"], "ml");
    }

    /// Turning TLS on has to move the probes too — an HTTP probe against a
    /// TLS listener fails, and the control plane would never become ready.
    #[test]
    fn tls_moves_the_probes_to_https_as_well_as_the_listener() {
        let mut cr = spec();
        cr.spec.control.tls_secret_name = Some("fastllm-control-tls".into());
        let d = resources::control_deployment(&cr, "h");
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
        let p = resources::proxy_deployment(&cr, "img", "h");
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

    /// The control-plane URL has to match what the certificate was issued
    /// for. cert-manager writes `<service>.<namespace>.svc`; the bare Service
    /// name is not a SAN, and a proxy that cannot complete the handshake
    /// serves its cached snapshot for ever without saying why.
    #[test]
    fn the_gateway_reaches_the_control_plane_by_its_certificate_name() {
        let cr = spec();
        let d = resources::proxy_deployment(&cr, "img", "h");
        let url = d.spec.unwrap().template.spec.unwrap().containers[0]
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "FASTLLM_CONTROL_URL")
            .unwrap()
            .value
            .clone()
            .unwrap();
        assert_eq!(url, "http://demo-control.fastllm.svc:4001/snapshot");
    }

    #[test]
    fn policy_reaches_the_flag_the_binary_accepts() {
        let mut cr = spec();
        cr.spec.proxy.policy = Policy::LowestLatency;
        let d = resources::proxy_deployment(&cr, "img", "h");
        let args = d.spec.unwrap().template.spec.unwrap().containers[0]
            .args
            .clone()
            .unwrap();
        assert!(args.contains(&"--policy=lowest-latency".to_string()));
    }

    /// The escape hatch has to come last, or it cannot override anything.
    #[test]
    fn extra_args_are_appended_after_everything_the_controller_computed() {
        let mut cr = spec();
        cr.spec.proxy.pod.extra_args = vec!["--max-body-bytes=1048576".into()];
        let d = resources::proxy_deployment(&cr, "img", "h");
        let args = d.spec.unwrap().template.spec.unwrap().containers[0]
            .args
            .clone()
            .unwrap();
        assert_eq!(args.last().unwrap(), "--max-body-bytes=1048576");
    }

    /// A fresh install with no models is not a broken install, and the
    /// condition has to say which one it is — otherwise `Degraded` sends
    /// somebody debugging a deployment that is behaving exactly as designed.
    #[test]
    fn an_unready_gateway_explains_the_commonest_reason() {
        let m = ready_message(true, 0, 2);
        assert!(m.contains("model backend is healthy"), "{m}");
        assert!(!m.contains("  "), "no stray whitespace from wrapping: {m}");
        assert_eq!(ready_message(true, 2, 2), "2 of 2 gateway replicas ready");
        assert_eq!(
            ready_message(false, 0, 2),
            "control plane has no ready replica"
        );
    }

    /// Everything the operator makes has to be garbage-collected with the
    /// resource that asked for it, including after the operator is gone.
    #[test]
    fn every_object_is_owned_by_the_resource_that_asked_for_it() {
        let mut cr = spec();
        cr.spec.proxy.ingress.enabled = true;
        cr.spec.proxy.autoscaling.enabled = true;
        cr.spec.bootstrap = Some(BootstrapSpec {
            name: "admin".into(),
            password: SecretRef {
                name: "s".into(),
                key: "p".into(),
            },
        });
        for meta in [
            resources::config_map(&cr).metadata,
            resources::control_deployment(&cr, "h").metadata,
            resources::proxy_deployment(&cr, "img", "h").metadata,
            resources::service(&cr, resources::PROXY).metadata,
            resources::pod_disruption_budget(&cr).unwrap().metadata,
            resources::horizontal_pod_autoscaler(&cr).unwrap().metadata,
            resources::ingress(&cr).unwrap().metadata,
            resources::bootstrap_job(&cr).unwrap().metadata,
        ] {
            let owners = meta.owner_references.expect("owner reference");
            assert_eq!(owners[0].kind, "FastllmProxy");
            assert_eq!(owners[0].controller, Some(true));
        }
    }
}
