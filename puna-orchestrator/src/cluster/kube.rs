//! The real cluster.
//!
//! The only file in the tree that talks to an API server, and the only implementation of
//! [`ClusterApi`] that renders a manifest. Everything decision-shaped happens above it, which is why
//! this file has almost no branches: it translates, and it classifies errors.
//!
//! ## The error classification is the interesting part
//!
//! §7's rule is that **the tick is the retry** (nothing loops inside one pass) so an error only has
//! to answer one question: try again next tick, or stop touching this room? Getting that wrong is
//! expensive in both directions. A `403` treated as transient is a room that fails identically every
//! 30 seconds forever, filling the log with the same line; a `503` treated as fatal is a room that
//! stays down after the API server comes back.
//!
//! ## Deletion is FOREGROUND, deliberately
//!
//! With background propagation the Deployment object disappears at once while its pod is still
//! draining, and "the Deployment is gone" would stop meaning "the room is not running". Two things
//! depend on it meaning that: the deletion sequence moves the room's state directory to the trash
//! once the Deployment is absent, and a restart that races a draining pod fails on pahoa's `flock`.
//! Foreground keeps the object until its dependents are gone, so absence is a real answer.
//!
//! The cost is a stuck finalizer showing up as a room parked in `stopping`. That is the better
//! failure: it is visible, it keeps the port reserved, and it does not move a directory a live
//! process is still writing to.

use async_trait::async_trait;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Secret, Service};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};

use super::{
    ClusterApi, ClusterError, IpamRefusal, Result, RoomDeployment, RoomSecret, RoomService,
    RoomSpec, SecretSpec, ServiceSpec,
};
use crate::spec::{self, Site};

/// Cilium's condition on a `LoadBalancer` Service, and the only one Puna reads.
///
/// Spelled once because it is a string contract with another project: a typo here reads exactly
/// like a cluster that never refuses anything, which is the failure mode this whole path exists to
/// remove.
const IPAM_REQUEST_SATISFIED: &str = "IPAMRequestSatisfied";

/// Count one API call, by what it did and how it went.
///
/// Wrapping every call rather than sampling: at a few requests per tick the cost is nothing, and the
/// question this answers ("is the orchestrator hammering the API server, and which verb") is
/// only answerable if nothing is missing from the denominator. `result` is coarse on purpose: `ok`,
/// `fatal`, `transient`, `conflict`. A per-status-code label would be a cardinality problem in
/// exchange for detail the error message already carries.
fn count<T>(verb: &str, resource: &str, outcome: Result<T>) -> Result<T> {
    let result = match &outcome {
        Ok(_) => "ok",
        Err(ClusterError::AlreadyExists { .. }) => "conflict",
        Err(e) if e.is_fatal() => "fatal",
        Err(_) => "transient",
    };
    puna_core::metrics::K8S_REQUESTS
        .with_label_values(&[verb, resource, result])
        .inc();
    outcome
}

/// The field manager server-side apply writes under.
///
/// Stable across restarts on purpose: a changing name would leave one `managedFields` entry per
/// orchestrator generation on every Secret in the namespace.
const FIELD_MANAGER: &str = "puna-orchestrator";

pub struct KubeCluster {
    deployments: Api<Deployment>,
    services: Api<Service>,
    secrets: Api<Secret>,
    site: Site,
}

impl KubeCluster {
    /// Connect using the pod's ServiceAccount, or a kubeconfig outside the cluster.
    pub async fn connect(site: Site) -> anyhow::Result<Self> {
        let client = kube::Client::try_default().await?;
        Ok(Self {
            deployments: Api::namespaced(client.clone(), &site.namespace),
            services: Api::namespaced(client.clone(), &site.namespace),
            secrets: Api::namespaced(client, &site.namespace),
            site,
        })
    }

    /// Every list is label-selected and served from the watch cache.
    ///
    /// `resourceVersion=0` is what makes three calls per tick cheap at several hundred rooms: the
    /// API server answers from memory rather than etcd. The price is that a read can lag, which is
    /// exactly why the planner waits out a grace period before believing a Deployment has vanished.
    fn list_params() -> ListParams {
        ListParams::default()
            .labels(&spec::managed_selector())
            .match_any()
    }
}

#[async_trait]
impl ClusterApi for KubeCluster {
    async fn list_deployments(&self) -> Result<Vec<RoomDeployment>> {
        let list = count(
            "list",
            "deployments",
            self.deployments
                .list(&Self::list_params())
                .await
                .map_err(classify),
        )?;
        Ok(list
            .items
            .iter()
            .filter_map(|d| read_deployment(d, &self.site.naming))
            .collect())
    }

    async fn list_services(&self) -> Result<Vec<RoomService>> {
        let list = count(
            "list",
            "services",
            self.services
                .list(&Self::list_params())
                .await
                .map_err(classify),
        )?;
        Ok(list
            .items
            .iter()
            .filter_map(|x| read_service(x, &self.site.naming))
            .collect())
    }

    async fn list_secrets(&self) -> Result<Vec<RoomSecret>> {
        let list = count(
            "list",
            "secrets",
            self.secrets
                .list(&Self::list_params())
                .await
                .map_err(classify),
        )?;
        Ok(list
            .items
            .iter()
            .filter_map(|x| read_secret(x, &self.site.naming))
            .collect())
    }

    async fn get_deployment(&self, name: &str) -> Result<Option<RoomDeployment>> {
        let found = count(
            "get",
            "deployments",
            self.deployments.get_opt(name).await.map_err(classify),
        )?;
        Ok(found
            .as_ref()
            .and_then(|d| read_deployment(d, &self.site.naming)))
    }

    async fn create_deployment(&self, spec: &RoomSpec) -> Result<String> {
        let manifest = spec::deployment::build(spec, &self.site);
        let created = count(
            "create",
            "deployments",
            self.deployments
                .create(&PostParams::default(), &manifest)
                .await
                .map_err(classify),
        )?;

        // The caller persists this before anything comes to depend on it. An object the API server
        // created always has one, so `None` here means the response was not what it claimed to be.
        created.metadata.uid.ok_or_else(|| {
            ClusterError::Fatal(format!(
                "{} was created without a uid; nothing could ever be owned by it",
                spec.name()
            ))
        })
    }

    async fn delete_deployment(&self, name: &str) -> Result<()> {
        // Foreground: see the module note. Absence has to mean the pod is gone.
        let outcome = match self
            .deployments
            .delete(name, &DeleteParams::foreground())
            .await
        {
            Ok(_) => Ok(()),
            // Already gone is what was wanted. Teardown runs every tick until the row is gone, so
            // anything else would turn a completed delete into an error that never clears.
            Err(kube::Error::Api(response)) if response.code == 404 => Ok(()),
            Err(e) => Err(classify(e)),
        };
        count("delete", "deployments", outcome)
    }

    async fn apply_secret(&self, spec: &SecretSpec) -> Result<()> {
        let manifest = spec::service::secret(spec, &self.site);
        // Server-side apply, forced: Puna owns every field of a room's Secret, and without `force` a
        // field another manager once touched would make every later apply conflict.
        count(
            "apply",
            "secrets",
            self.secrets
                .patch(
                    &spec.name(),
                    &PatchParams::apply(FIELD_MANAGER).force(),
                    &Patch::Apply(&manifest),
                )
                .await
                .map_err(classify),
        )?;
        Ok(())
    }

    async fn delete_secret(&self, name: &str) -> Result<()> {
        let outcome = match self.secrets.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(response)) if response.code == 404 => Ok(()),
            Err(e) => Err(classify(e)),
        };
        count("delete", "secrets", outcome)
    }

    async fn create_service(&self, spec: &ServiceSpec) -> Result<()> {
        let manifest = spec::service::build(spec, &self.site);
        count(
            "create",
            "services",
            self.services
                .create(&PostParams::default(), &manifest)
                .await
                .map_err(classify),
        )?;
        Ok(())
    }

    async fn get_service(&self, name: &str) -> Result<Option<RoomService>> {
        let found = count(
            "get",
            "services",
            self.services.get_opt(name).await.map_err(classify),
        )?;
        Ok(found
            .as_ref()
            .and_then(|x| read_service(x, &self.site.naming)))
    }
}

/// Which errors are worth trying again.
fn classify(error: kube::Error) -> ClusterError {
    let kube::Error::Api(response) = &error else {
        // Transport, TLS, a token refresh: all things that come back on their own.
        return ClusterError::Transient(error.to_string());
    };

    let message = format!("{} ({})", response.message, response.code);
    match response.code {
        // The name is carried in the message rather than a field: `ErrorResponse` keeps only status,
        // message, reason and code, and the caller already knows which object it asked for.
        409 => ClusterError::AlreadyExists {
            name: response.message.clone(),
        },
        // RBAC, or a request the API server will refuse identically forever.
        401 | 403 => ClusterError::Fatal(message),
        // A `404` here is not "the object is missing": `get_opt` returns `None` for that and the
        // deletes above treat it as success. Reaching this means the *resource type* was not found:
        // a wrong apiVersion, or a CRD that is not installed.
        404 => ClusterError::Fatal(format!("{message}: is the apiVersion right?")),
        // The manifest itself is wrong. Retrying sends the same bytes.
        400 | 422 => ClusterError::Fatal(message),
        _ => ClusterError::Transient(message),
    }
}

fn read_deployment(deployment: &Deployment, naming: &spec::Naming) -> Option<RoomDeployment> {
    let metadata = &deployment.metadata;
    // An object with no name or uid cannot be acted on or owned. Skipping keeps the snapshot a set
    // of things that can be reasoned about, rather than one with holes in it.
    let (name, uid) = (metadata.name.clone()?, metadata.uid.clone()?);
    let status = deployment.status.as_ref();

    Some(RoomDeployment {
        name,
        uid,
        room_id: metadata.labels.as_ref().and_then(|l| naming.room_of(l)),
        spec_hash: metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(&naming.spec_hash_annotation))
            .cloned(),
        // By container NAME, not `containers[0]`: an injected sidecar would take that slot and the
        // admin table would confidently report the wrong image. No match is `None`.
        image: deployment
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|pod| {
                pod.containers
                    .iter()
                    .find(|c| c.name == spec::ROOM_CONTAINER)
            })
            .and_then(|c| c.image.clone()),
        replicas: status.and_then(|s| s.replicas).unwrap_or(0),
        // Absent means zero here, unlike a probe's `None`: the field is omitted until a replica is
        // ready, and "no ready replica" is exactly what that means.
        ready_replicas: status.and_then(|s| s.ready_replicas).unwrap_or(0),
        created_at: metadata
            .creation_timestamp
            .as_ref()
            .map(|time| time.0)
            .unwrap_or_else(chrono::Utc::now),
        // Presence is the whole signal; the value is when the delete was accepted, which nothing
        // needs. Kubernetes sets it on the object and leaves it readable until the finalizers
        // clear, so this reads `true` for exactly as long as the old pod is still draining.
        deleting: metadata.deletion_timestamp.is_some(),
    })
}

fn read_service(service: &Service, naming: &spec::Naming) -> Option<RoomService> {
    let metadata = &service.metadata;
    Some(RoomService {
        name: metadata.name.clone()?,
        room_id: metadata.labels.as_ref().and_then(|l| naming.room_of(l)),
        ingress_ip: service
            .status
            .as_ref()
            .and_then(|s| s.load_balancer.as_ref())
            .and_then(|lb| lb.ingress.as_ref())
            .and_then(|ingress| ingress.first())
            .and_then(|ingress| ingress.ip.clone()),
        // Only an explicit `False` counts. A missing condition (an older Cilium, or a Service the
        // operator has not reached yet) reads as `None` and leaves the room waiting, which is the
        // behavior this had before the condition was read at all.
        ipam_refusal: service
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|conditions| {
                conditions
                    .iter()
                    .find(|c| c.type_ == IPAM_REQUEST_SATISFIED)
            })
            .filter(|c| c.status == "False")
            .map(|c| IpamRefusal {
                reason: c.reason.clone(),
                message: c.message.clone(),
            }),
        ports: service
            .spec
            .as_ref()
            .and_then(|s| s.ports.as_ref())
            .map(|ports| {
                ports
                    .iter()
                    .filter_map(|p| u16::try_from(p.port).ok())
                    .collect()
            })
            .unwrap_or_default(),
        owner_uid: controller_uid(metadata),
    })
}

fn read_secret(secret: &Secret, naming: &spec::Naming) -> Option<RoomSecret> {
    let metadata = &secret.metadata;
    Some(RoomSecret {
        name: metadata.name.clone()?,
        room_id: metadata.labels.as_ref().and_then(|l| naming.room_of(l)),
        owner_uid: controller_uid(metadata),
    })
}

/// The uid of the object that owns this one, if anything does.
///
/// `None` is the leak the sweep looks for: nothing will ever garbage-collect it.
fn controller_uid(
    metadata: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> Option<String> {
    metadata
        .owner_references
        .as_ref()?
        .iter()
        .find(|owner| owner.controller == Some(true))
        .map(|owner| owner.uid.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naming() -> spec::Naming {
        spec::Naming {
            room_key: "example.test/room".into(),
            lb_pool_key: "example.test/lb-pool".into(),
            lb_pool: "public".into(),
            spec_hash_annotation: "puna.example.test/spec-hash".into(),
        }
    }

    use k8s_openapi::api::apps::v1::DeploymentStatus;
    use k8s_openapi::api::core::v1::{LoadBalancerIngress, LoadBalancerStatus, ServiceStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
    use kube::core::ErrorResponse;
    use puna_core::ids::RoomId;

    fn api_error(code: u16, message: &str) -> kube::Error {
        kube::Error::Api(ErrorResponse {
            status: "Failure".into(),
            message: message.into(),
            reason: String::new(),
            code,
        })
    }

    /// The classification that decides between "one bad tick" and "a rolling outage".
    #[test]
    fn errors_are_classified_by_whether_retrying_could_help() {
        assert!(matches!(
            classify(api_error(409, "already exists")),
            ClusterError::AlreadyExists { .. }
        ));

        for code in [401, 403] {
            assert!(
                classify(api_error(code, "forbidden")).is_fatal(),
                "{code} cannot be fixed by waiting 30 seconds"
            );
        }
        // A wrong apiVersion, not a missing object: `get_opt` answers None and the deletes treat 404
        // as success, so anything reaching here is the resource type.
        assert!(classify(api_error(404, "the server could not find")).is_fatal());
        // The manifest is wrong; a retry sends the same bytes.
        assert!(classify(api_error(422, "Deployment is invalid")).is_fatal());

        for code in [429, 500, 502, 503, 504] {
            let error = classify(api_error(code, "try later"));
            assert!(!error.is_fatal(), "{code} is worth another tick");
            assert!(matches!(error, ClusterError::Transient(_)));
        }
    }

    /// The message has to survive, because it is the only thing an operator gets.
    #[test]
    fn a_classified_error_still_says_what_the_api_server_said() {
        let error = classify(api_error(403, "secrets is forbidden: RBAC"));
        assert!(
            error.to_string().contains("secrets is forbidden"),
            "{error}"
        );
        assert!(error.to_string().contains("403"), "{error}");
    }

    #[test]
    fn a_deployment_is_read_down_to_the_seven_fields_the_reconciler_uses() {
        let room = RoomId::new();
        let deployment = Deployment {
            metadata: ObjectMeta {
                name: Some(format!("mw-{room}")),
                uid: Some("uid-1".into()),
                labels: Some(naming().labels(room)),
                annotations: Some(std::collections::BTreeMap::from([(
                    naming().spec_hash_annotation.clone(),
                    "hash-1".to_string(),
                )])),
                ..Default::default()
            },
            status: Some(DeploymentStatus {
                replicas: Some(1),
                ready_replicas: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };

        let read = read_deployment(&deployment, &naming()).expect("readable");
        assert_eq!(read.room_id, Some(room));
        assert_eq!(read.uid, "uid-1");
        assert_eq!(read.spec_hash.as_deref(), Some("hash-1"));
        assert_eq!((read.replicas, read.ready_replicas), (1, 1));
    }

    /// A Deployment whose pod is not up yet omits `readyReplicas` entirely, and that omission means
    /// zero rather than "cannot tell": the opposite of how a probe's absent field reads.
    #[test]
    fn an_absent_ready_replica_count_is_zero() {
        let deployment = Deployment {
            metadata: ObjectMeta {
                name: Some("mw-x".into()),
                uid: Some("uid-1".into()),
                ..Default::default()
            },
            status: Some(DeploymentStatus {
                replicas: Some(1),
                ready_replicas: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            read_deployment(&deployment, &naming())
                .unwrap()
                .ready_replicas,
            0
        );
    }

    /// An object Puna cannot own or address is left out of the snapshot rather than half-read.
    #[test]
    fn an_object_with_no_uid_is_not_in_the_snapshot() {
        let deployment = Deployment {
            metadata: ObjectMeta {
                name: Some("mw-x".into()),
                uid: None,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(read_deployment(&deployment, &naming()).is_none());
    }

    /// A Deployment with our label and no parseable room is an orphan by definition.
    #[test]
    fn an_unlabeled_deployment_belongs_to_no_room() {
        let deployment = Deployment {
            metadata: ObjectMeta {
                name: Some("mw-x".into()),
                uid: Some("uid-1".into()),
                labels: Some(std::collections::BTreeMap::from([(
                    spec::MANAGED_BY_KEY.to_string(),
                    spec::MANAGED_BY.to_string(),
                )])),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            read_deployment(&deployment, &naming()).unwrap().room_id,
            None
        );
    }

    #[test]
    fn a_service_reports_its_address_only_once_ipam_has_answered() {
        let room = RoomId::new();
        let mut service = Service {
            metadata: ObjectMeta {
                name: Some(format!("mw-{room}")),
                labels: Some(naming().labels(room)),
                owner_references: Some(vec![OwnerReference {
                    api_version: "apps/v1".into(),
                    kind: "Deployment".into(),
                    name: format!("mw-{room}"),
                    uid: "uid-1".into(),
                    controller: Some(true),
                    block_owner_deletion: None,
                }]),
                ..Default::default()
            },
            ..Default::default()
        };

        let pending = read_service(&service, &naming()).expect("readable");
        assert_eq!(pending.ingress_ip, None, "IPAM has not answered yet");
        assert_eq!(pending.owner_uid.as_deref(), Some("uid-1"));
        assert_eq!(pending.room_id, Some(room));

        service.status = Some(ServiceStatus {
            load_balancer: Some(LoadBalancerStatus {
                ingress: Some(vec![LoadBalancerIngress {
                    ip: Some("192.0.2.10".into()),
                    ..Default::default()
                }]),
            }),
            ..Default::default()
        });
        assert_eq!(
            read_service(&service, &naming())
                .unwrap()
                .ingress_ip
                .as_deref(),
            Some("192.0.2.10")
        );
    }

    /// The leak the sweep exists to find: nothing will ever collect this.
    #[test]
    fn an_object_with_no_controller_owner_reports_none() {
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some("mw-x".into()),
                owner_references: Some(vec![OwnerReference {
                    api_version: "apps/v1".into(),
                    kind: "Deployment".into(),
                    name: "mw-x".into(),
                    uid: "uid-1".into(),
                    // Present but not the controller: the garbage collector's cascade follows the
                    // controller reference, so this does not make the Secret collectable.
                    controller: None,
                    block_owner_deletion: None,
                }]),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(read_secret(&secret, &naming()).unwrap().owner_uid, None);
    }

    /// Three list calls per tick regardless of room count, and both properties matter: the label
    /// selector is what makes "orphan" mean "ours with no room", and `resourceVersion=0` is what
    /// keeps the calls off etcd.
    #[test]
    fn lists_are_label_selected_and_served_from_the_watch_cache() {
        let params = KubeCluster::list_params();
        assert_eq!(
            params.label_selector.as_deref(),
            Some("app.kubernetes.io/managed-by=puna")
        );
        assert_eq!(params.resource_version.as_deref(), Some("0"));
    }
}
