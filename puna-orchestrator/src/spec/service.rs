//! A room's LoadBalancer Service, and the Secret's manifest.
//!
//! ## The annotations are the design, not decoration
//!
//! A Service cannot express a port range, which is the fact that makes an orchestrator necessary at
//! all: something has to create one Service per room at runtime. Cilium's `sharing-key` is what lets
//! hundreds of them share one public address as long as their ports stay distinct — measured at 300
//! Services on one key, all landing on the same IP.
//!
//! **The failure mode is silent, and every annotation here is aimed at it.** Cilium does not error
//! when sharing degrades; its own documentation says conflicting Services "will be allocated
//! different IPs". A room would come up perfectly healthy on an address DNS never mentions, so the
//! applier reads `status.loadBalancer.ingress` back and quarantines the pair on a mismatch.
//!
//! Two of the four are easy to omit and both have been:
//!
//!   * **`sharing-cross-namespace`** must be on *both* sides. The lobby's Gateway already carries
//!     `"*"`; without it here, sharing simply does not happen across the namespace boundary.
//!   * **`allocateLoadBalancerNodePorts: false`**. The default allocates a NodePort per port, and the
//!     range holds 2768 — two per room means it runs out at around 1400 rooms, on a design whose
//!     port ranges allow 5000.
use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Secret, Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::cluster::{OwnerRef, SecretSpec, ServiceSpec as RoomServiceSpec};
use crate::spec::deployment::{PORT_FILTERED, PORT_FULL};
use crate::spec::{LB_POOL, LB_POOL_KEY, Site};

const SHARING_KEY_ANNOTATION: &str = "lbipam.cilium.io/sharing-key";
const SHARING_CROSS_NAMESPACE_ANNOTATION: &str = "lbipam.cilium.io/sharing-cross-namespace";
const REQUESTED_IPS_ANNOTATION: &str = "lbipam.cilium.io/ips";

pub fn build(spec: &RoomServiceSpec, site: &Site) -> Service {
    let mut labels = crate::spec::labels(spec.room_id);
    // Required, not decorative: the `br-v5` pool's NotIn selector matches anything without it, and
    // the Service lands on a private 172.29.0.x address the room is unreachable from.
    labels.insert(LB_POOL_KEY.to_string(), LB_POOL.to_string());

    let mut ports = vec![ServicePort {
        name: Some(PORT_FULL.to_string()),
        port: i32::from(spec.base_port),
        // By name, so the Service and the container cannot disagree about which listener is which.
        target_port: Some(IntOrString::String(PORT_FULL.to_string())),
        protocol: Some("TCP".to_string()),
        ..Default::default()
    }];
    if spec.wants_filtered {
        ports.push(ServicePort {
            name: Some(PORT_FILTERED.to_string()),
            port: i32::from(spec.base_port) + 1,
            target_port: Some(IntOrString::String(PORT_FILTERED.to_string())),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        });
    }

    Service {
        metadata: ObjectMeta {
            name: Some(spec.name()),
            namespace: Some(site.namespace.clone()),
            labels: Some(labels),
            annotations: Some(BTreeMap::from([
                (
                    SHARING_KEY_ANNOTATION.to_string(),
                    site.lb_sharing_key.clone(),
                ),
                (
                    SHARING_CROSS_NAMESPACE_ANNOTATION.to_string(),
                    "*".to_string(),
                ),
                (REQUESTED_IPS_ANNOTATION.to_string(), site.lb_ip.clone()),
            ])),
            owner_references: Some(vec![owner_reference(&spec.owner)]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("LoadBalancer".to_string()),
            allocate_load_balancer_node_ports: Some(false),
            // The only pool matching `lb-pool: public` has no v6 block, so asking for dual-stack
            // leaves the Service pending forever.
            ip_family_policy: Some("SingleStack".to_string()),
            ip_families: Some(vec!["IPv4".to_string()]),
            selector: Some(crate::spec::selector_labels(spec.room_id)),
            ports: Some(ports),
            // `externalTrafficPolicy` deliberately unset: Cluster is correct under DSR, and Local
            // would drop traffic arriving at a node not running this room's single pod.
            ..Default::default()
        }),
        status: None,
    }
}

/// The room's Secret: the environment `spec::secret::build` produced, as an object.
pub fn secret(spec: &SecretSpec, site: &Site) -> Secret {
    Secret {
        metadata: ObjectMeta {
            name: Some(spec.name()),
            namespace: Some(site.namespace.clone()),
            labels: Some(crate::spec::labels(spec.room_id)),
            // `None` on the first apply, because the Deployment it would point at does not exist
            // yet — the pod cannot start without the Secret, so the Secret cannot wait for the pod.
            owner_references: spec
                .owner
                .as_ref()
                .map(|owner| vec![owner_reference(owner)]),
            ..Default::default()
        },
        // `string_data` rather than `data`: the API server base64-encodes it, so nothing here has to
        // encode, and a mistake in encoding would surface as a password nobody can use.
        string_data: Some(spec.data.clone()),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

/// The ownerReference that makes teardown one call.
///
/// **`blockOwnerDeletion` is deliberately omitted.** Setting it triggers the
/// `OwnerReferencesPermissionEnforcement` admission plugin, which checks for `update` on the owner's
/// `finalizers` subresource — a verb the orchestrator's Role does not grant and should not.
fn owner_reference(owner: &OwnerRef) -> OwnerReference {
    OwnerReference {
        api_version: "apps/v1".to_string(),
        kind: "Deployment".to_string(),
        name: owner.name.clone(),
        // Required. Without it the garbage collector never matches an owner, and the object is
        // simply never collected: a Service holding a port for a room that no longer exists.
        uid: owner.uid.clone(),
        controller: Some(true),
        block_owner_deletion: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::object_name;
    use puna_core::ids::RoomId;

    fn site() -> Site {
        Site {
            namespace: "puna-dev".into(),
            lb_ip: "38.246.56.121".into(),
            lb_sharing_key: "ap-lobby-public".into(),
            tls_secret: "puna-room-tls".into(),
            data_pvc: "puna-data".into(),
        }
    }

    fn owner(room: RoomId) -> OwnerRef {
        OwnerRef {
            name: object_name(room),
            uid: "uid-1".into(),
        }
    }

    fn spec(room: RoomId) -> RoomServiceSpec {
        RoomServiceSpec {
            room_id: room,
            base_port: 40000,
            wants_filtered: true,
            owner: owner(room),
        }
    }

    #[test]
    fn the_four_ipam_annotations_are_all_present() {
        let room = RoomId::new();
        let service = build(&spec(room), &site());
        let annotations = service.metadata.annotations.clone().unwrap();

        assert_eq!(annotations[SHARING_KEY_ANNOTATION], "ap-lobby-public");
        assert_eq!(annotations[SHARING_CROSS_NAMESPACE_ANNOTATION], "*");
        assert_eq!(annotations[REQUESTED_IPS_ANNOTATION], "38.246.56.121");
        assert_eq!(
            service.metadata.labels.as_ref().unwrap()[LB_POOL_KEY],
            LB_POOL
        );
    }

    /// Two per room against a 2768-port range: the default would exhaust at about 1400 rooms, on
    /// port ranges that allow 5000.
    #[test]
    fn node_ports_are_not_allocated_and_the_family_is_v4_only() {
        let room = RoomId::new();
        let spec = build(&spec(room), &site()).spec.unwrap();

        assert_eq!(spec.type_.as_deref(), Some("LoadBalancer"));
        assert_eq!(spec.allocate_load_balancer_node_ports, Some(false));
        assert_eq!(spec.ip_family_policy.as_deref(), Some("SingleStack"));
        assert_eq!(spec.ip_families, Some(vec!["IPv4".to_string()]));
        // Unset on purpose: Local would drop traffic at any node not running this room's one pod.
        assert!(spec.external_traffic_policy.is_none());
    }

    /// The other half of the selector agreement. A mismatch here is a Service with no endpoints:
    /// connection refused on a room Kubernetes reports as healthy.
    #[test]
    fn the_service_selects_the_pods_the_deployment_creates() {
        let room = RoomId::new();
        let service = build(&spec(room), &site());
        let deployment = crate::spec::deployment::build(
            &crate::cluster::RoomSpec {
                room_id: room,
                spec_hash: "f00d".into(),
                image: "pahoa:test".into(),
                base_port: 40000,
                wants_filtered: true,
                slot_count: 4,
                save_interval_secs: 30,
                use_embedded_options: true,
            },
            &site(),
        );

        assert_eq!(
            service.spec.unwrap().selector,
            deployment.spec.unwrap().selector.match_labels
        );
    }

    #[test]
    fn both_ports_are_published_and_target_the_container_by_name() {
        let room = RoomId::new();
        let mut spec = spec(room);
        let ports = build(&spec, &site()).spec.unwrap().ports.unwrap();
        assert_eq!(
            ports
                .iter()
                .map(|p| (
                    p.name.clone().unwrap(),
                    p.port,
                    p.target_port.clone().unwrap()
                ))
                .collect::<Vec<_>>(),
            [
                (
                    PORT_FULL.to_string(),
                    40000,
                    IntOrString::String(PORT_FULL.to_string())
                ),
                (
                    PORT_FILTERED.to_string(),
                    40001,
                    IntOrString::String(PORT_FILTERED.to_string())
                ),
            ]
        );

        // The pair stays reserved when the filtered feed is off; only the second listener goes.
        spec.wants_filtered = false;
        let ports = build(&spec, &site()).spec.unwrap().ports.unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, 40000);
    }

    /// The uid is what makes teardown work, and omitting `blockOwnerDeletion` is what makes it work
    /// with the Role the orchestrator actually has.
    #[test]
    fn the_owner_reference_carries_a_uid_and_no_deletion_block() {
        let room = RoomId::new();
        let service = build(&spec(room), &site());
        let owners = service.metadata.owner_references.clone().unwrap();

        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].uid, "uid-1");
        assert_eq!(owners[0].kind, "Deployment");
        assert_eq!(owners[0].api_version, "apps/v1");
        assert_eq!(owners[0].controller, Some(true));
        assert_eq!(
            owners[0].block_owner_deletion, None,
            "setting it needs `update` on the owner's finalizers subresource, which the Role does \
             not grant"
        );
    }

    /// §7 applies the Secret twice on purpose, and the difference between the two is the owner.
    #[test]
    fn the_secret_is_unowned_on_the_first_apply_and_owned_on_the_second() {
        let room = RoomId::new();
        let mut data = crate::spec::secret::SecretData::new();
        data.insert("PAHOA_ADMIN_TOKEN".into(), "a".repeat(52));

        let first = secret(
            &SecretSpec {
                room_id: room,
                data: data.clone(),
                owner: None,
            },
            &site(),
        );
        assert!(
            first.metadata.owner_references.is_none(),
            "the Deployment it would point at does not exist yet"
        );

        let second = secret(
            &SecretSpec {
                room_id: room,
                data: data.clone(),
                owner: Some(owner(room)),
            },
            &site(),
        );
        assert_eq!(second.metadata.owner_references.unwrap()[0].uid, "uid-1");

        // Same object both times, so the second apply is an update rather than a second Secret.
        assert_eq!(first.metadata.name, second.metadata.name);
        assert_eq!(second.type_.as_deref(), Some("Opaque"));
        assert_eq!(second.string_data.as_ref().unwrap(), &data);
        assert!(
            second.data.is_none(),
            "stringData is what the API server encodes; encoding it here twice would corrupt it"
        );
    }

    /// Every room object is Puna's and says so, or the sweep cannot see it.
    #[test]
    fn every_object_is_labeled_for_its_room_and_managed_by_puna() {
        let room = RoomId::new();
        let mut data = crate::spec::secret::SecretData::new();
        data.insert("PAHOA_ADMIN_TOKEN".into(), "t".into());

        let service = build(&spec(room), &site());
        let secret = secret(
            &SecretSpec {
                room_id: room,
                data,
                owner: None,
            },
            &site(),
        );

        for labels in [
            service.metadata.labels.clone().unwrap(),
            secret.metadata.labels.clone().unwrap(),
        ] {
            assert_eq!(crate::spec::room_of(&labels), Some(room));
            assert_eq!(labels[crate::spec::MANAGED_BY_KEY], crate::spec::MANAGED_BY);
        }
        assert_eq!(service.metadata.namespace, secret.metadata.namespace);
    }
}
