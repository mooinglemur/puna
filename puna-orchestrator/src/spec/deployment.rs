//! A room's Deployment.
//!
//! The manifest, in Kubernetes' own vocabulary. This is the one place `k8s_openapi` types appear in
//! `spec::`, and it is a pure function: [`build`] takes a [`RoomSpec`] and a [`Site`] and returns a
//! `Deployment`, so every field below is asserted in a unit test rather than in a cluster.
//!
//! ## Four fields that look like boilerplate and are not
//!
//! **`strategy: Recreate`.** Pahoa holds an exclusive `flock` on the save directory for as long as
//! the process runs, so a RollingUpdate's surge pod cannot start — it CrashLoopBackOffs until
//! `progressDeadlineSeconds` expires and the rollout is reported as failed. Recreate is not a
//! preference; it is the only strategy that works.
//!
//! **`limits.cpu`.** Pahoa sizes its Tokio pool from the cgroup CPU quota, falling back to *host*
//! parallelism when there is none. Omit the limit and a five-slot room on a 64-core node spawns 64
//! worker threads. The startup banner reports `cpu_quota`, so its absence there is how this mistake
//! announces itself.
//!
//! **`enableServiceLinks: false`.** The default injects an environment variable pair for every
//! Service in the namespace, and this namespace accumulates one Service per room — hundreds of
//! variables in every room's environment, for nothing.
//!
//! **`terminationGracePeriodSeconds: 45`**, not the 30-second default. Pahoa's SIGTERM path budgets
//! about twenty seconds of disk: up to `shutdown_timeout` waiting out an in-flight save so the newest
//! snapshot lands last, then up to ten more to encode, write and fsync the final one. Thirty fits
//! with almost no margin, and the case that eats it is a CephFS MDS failover, where I/O blocks and no
//! userspace timeout helps. Overrunning means SIGKILL and a fall back to the last completed save —
//! exactly the loss pahoa's SIGTERM handling exists to remove.
use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EnvFromSource, EnvVar, EnvVarSource, HTTPGetAction,
    ObjectFieldSelector, PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec,
    PodTemplateSpec, Probe, ResourceRequirements, SeccompProfile, SecretEnvSource,
    SecretVolumeSource, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::cluster::{RoomSpec, object_name};
use crate::spec::{ROOM_SERVICE_ACCOUNT, SAVE_DIR, SPEC_HASH_ANNOTATION, Site, TLS_DIR};

/// The full feed. Named, because the Service and both probes refer to it by name.
pub const PORT_FULL: &str = "game-full";
/// The scoped feed: same server, same TLS, same HTTP surface, minus the item traffic of slots a
/// client is not playing.
pub const PORT_FILTERED: &str = "game-filtered";

/// Five minutes, matching the `startupProbe`'s own budget: a cold start is an image pull plus a save
/// restored from CephFS.
const PROGRESS_DEADLINE_SECS: i32 = 300;

/// See the module note. The one lever Puna has over pahoa's drain time.
const TERMINATION_GRACE_SECS: i64 = 45;

/// Hundreds of rooms, so no ReplicaSet history is kept at all. Rollback is "start it again", since
/// the room's state is on the volume rather than in the pod.
const REVISION_HISTORY_LIMIT: i32 = 0;

/// `0440`, as octal, and it has to be.
///
/// A Secret volume's files are owned `root:fsGroup`, so `0400` would be readable only by root while
/// the room runs as uid 1000. The image is `FROM scratch` with no `/etc/passwd`, so this mode is the
/// only thing making the certificate readable — and pahoa's failure is a fatal startup error naming
/// the path, which reads like a missing file rather than a permission.
const TLS_FILE_MODE: i32 = 0o440;

pub fn build(spec: &RoomSpec, site: &Site) -> Deployment {
    let name = object_name(spec.room_id);
    let labels = crate::spec::labels(spec.room_id);

    Deployment {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(site.namespace.clone()),
            labels: Some(labels.clone()),
            annotations: Some(BTreeMap::from([(
                SPEC_HASH_ANNOTATION.to_string(),
                spec.spec_hash.clone(),
            )])),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            revision_history_limit: Some(REVISION_HISTORY_LIMIT),
            progress_deadline_seconds: Some(PROGRESS_DEADLINE_SECS),
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".to_string()),
                rolling_update: None,
            }),
            selector: LabelSelector {
                match_labels: Some(crate::spec::selector_labels(spec.room_id)),
                match_expressions: None,
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(pod_spec(spec, site)),
            },
            ..Default::default()
        }),
        status: None,
    }
}

fn pod_spec(spec: &RoomSpec, site: &Site) -> PodSpec {
    PodSpec {
        service_account_name: Some(ROOM_SERVICE_ACCOUNT.to_string()),
        // The tier split, mechanically: a room cannot reach the API server even in principle.
        automount_service_account_token: Some(false),
        enable_service_links: Some(false),
        termination_grace_period_seconds: Some(TERMINATION_GRACE_SECS),
        security_context: Some(PodSecurityContext {
            run_as_non_root: Some(true),
            run_as_user: Some(1000),
            run_as_group: Some(1000),
            // Without this the Secret volume's files are unreadable by uid 1000, and the room dies
            // at startup unable to read its own certificate.
            fs_group: Some(1000),
            seccomp_profile: Some(SeccompProfile {
                type_: "RuntimeDefault".to_string(),
                localhost_profile: None,
            }),
            ..Default::default()
        }),
        containers: vec![container(spec)],
        volumes: Some(vec![
            Volume {
                name: "data".to_string(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: site.data_pvc.clone(),
                    read_only: None,
                }),
                ..Default::default()
            },
            Volume {
                name: "tls".to_string(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(site.tls_secret.clone()),
                    default_mode: Some(TLS_FILE_MODE),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]),
        ..Default::default()
    }
}

/// Everything in the container is the room's own; the [`Site`] only reaches the volumes above.
fn container(spec: &RoomSpec) -> Container {
    let mut ports = vec![ContainerPort {
        name: Some(PORT_FULL.to_string()),
        container_port: i32::from(spec.base_port),
        protocol: Some("TCP".to_string()),
        ..Default::default()
    }];
    if spec.wants_filtered {
        ports.push(ContainerPort {
            name: Some(PORT_FILTERED.to_string()),
            container_port: i32::from(spec.base_port) + 1,
            protocol: Some("TCP".to_string()),
            ..Default::default()
        });
    }

    Container {
        name: crate::spec::ROOM_CONTAINER.to_string(),
        image: Some(spec.image.clone()),
        // Exec form against a scratch image: there is no shell to expand anything, which is also why
        // nothing here can be a quoted string with a variable in it.
        args: Some(crate::spec::args::serve(spec)),
        // Every credential, by reference. The pod spec names a Secret and never a value.
        env_from: Some(vec![EnvFromSource {
            secret_ref: Some(SecretEnvSource {
                name: object_name(spec.room_id),
                optional: None,
            }),
            ..Default::default()
        }]),
        env: Some(downward_api()),
        ports: Some(ports),
        resources: Some(resources(spec)),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            read_only_root_filesystem: Some(true),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                add: None,
            }),
            ..Default::default()
        }),
        volume_mounts: Some(vec![
            VolumeMount {
                name: "data".to_string(),
                mount_path: SAVE_DIR.to_string(),
                // One room's directory and nothing else on the volume: a room cannot read another
                // room's save, or any generation.
                sub_path: Some(format!("rooms/{}", spec.room_id)),
                ..Default::default()
            },
            VolumeMount {
                name: "tls".to_string(),
                mount_path: TLS_DIR.to_string(),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        // Both probes are HTTPS on the game port, because pahoa terminates its own TLS and serves
        // /healthz on the same port as the WebSocket -- it sniffs the first byte to tell them apart.
        // The kubelet does not validate the certificate for probes, so this works regardless of the
        // name it dials, which is just as well: it dials a pod IP.
        startup_probe: Some(probe(60)),
        readiness_probe: Some(probe(3)),
        ..Default::default()
    }
}

/// `pod`, `namespace` and `node` for pahoa's startup banner.
///
/// Kubernetes sets none of these on its own, and without them a room's log cannot say where it ran —
/// which is the question a post-mortem starts with, after the pod is gone. Not secrets, so they are
/// plain `env` rather than part of the Secret.
fn downward_api() -> Vec<EnvVar> {
    [
        ("POD_NAME", "metadata.name"),
        ("POD_NAMESPACE", "metadata.namespace"),
        ("NODE_NAME", "spec.nodeName"),
    ]
    .into_iter()
    .map(|(name, field_path)| EnvVar {
        name: name.to_string(),
        value: None,
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                field_path: field_path.to_string(),
                api_version: None,
            }),
            ..Default::default()
        }),
    })
    .collect()
}

fn probe(failure_threshold: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/healthz".to_string()),
            // By name, not by number: the port moves with every reallocation and the name does not.
            port: IntOrString::String(PORT_FULL.to_string()),
            scheme: Some("HTTPS".to_string()),
            ..Default::default()
        }),
        period_seconds: Some(5),
        failure_threshold: Some(failure_threshold),
        ..Default::default()
    }
}

fn resources(spec: &RoomSpec) -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            // 50m, lowered from 250m before the first real deployment. A room is idle almost all of
            // the time: it holds sockets, applies the occasional check and writes a save every 30
            // seconds. The bursts that matter -- a seed loading, a wave of clients reconnecting --
            // are what `limits.cpu` covers, and a request is a RESERVATION rather than a ceiling.
            //
            // The request is what the scheduler subtracts from a node and what a ResourceQuota
            // charges, so setting it to steady-state demand rather than to burst demand is what
            // decides how many rooms fit on the fleet. At 250m a hundred rooms reserved 25 cores
            // that were doing nothing; at 50m they reserve 5 and still burst to 2 each.
            //
            // Revise it from measurement, not from argument -- container_cpu_usage_seconds_total
            // by pod over a week of real rooms. Too LOW shows up as rooms landing on a node that
            // cannot actually feed them, which reads as lag rather than as a scheduling fault.
            ("cpu".to_string(), Quantity("50m".to_string())),
            (
                "memory".to_string(),
                quantity_bytes(crate::spec::room::memory_request_bytes(spec.slot_count)),
            ),
        ])),
        limits: Some(BTreeMap::from([
            // Mandatory, and the module note says why: without it pahoa reads the node's core count.
            ("cpu".to_string(), Quantity("2".to_string())),
            (
                "memory".to_string(),
                quantity_bytes(crate::spec::room::memory_limit_bytes(spec.slot_count)),
            ),
        ])),
        ..Default::default()
    }
}

/// Bytes as a Quantity, in `Ki` where that is exact.
///
/// A bare integer is bytes and always correct, but a manifest is also read by people during an
/// incident, and `576000Ki` is legible where `589824000` is not. Falls back to bytes rather than
/// rounding: a resource request that quietly differs from what the formula said would make the
/// formula's tests meaningless.
fn quantity_bytes(bytes: i64) -> Quantity {
    if bytes % 1024 == 0 {
        Quantity(format!("{}Ki", bytes / 1024))
    } else {
        Quantity(bytes.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn spec() -> RoomSpec {
        RoomSpec {
            room_id: RoomId::new(),
            spec_hash: "f00d".into(),
            image: "registry.example/pahoa:sha-abc123".into(),
            base_port: 40000,
            wants_filtered: true,
            slot_count: 96,
            save_interval_secs: 30,
            use_embedded_options: true,
        }
    }

    fn pod(deployment: &Deployment) -> &PodSpec {
        deployment
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("a pod spec")
    }

    fn pahoa(deployment: &Deployment) -> &Container {
        &pod(deployment).containers[0]
    }

    #[test]
    fn the_room_is_named_and_labeled_for_its_id() {
        let spec = spec();
        let deployment = build(&spec, &site());
        let name = object_name(spec.room_id);

        assert_eq!(deployment.metadata.name.as_deref(), Some(name.as_str()));
        assert_eq!(deployment.metadata.namespace.as_deref(), Some("puna-dev"));
        assert_eq!(
            crate::spec::room_of(deployment.metadata.labels.as_ref().unwrap()),
            Some(spec.room_id)
        );
        assert_eq!(
            deployment.metadata.annotations.as_ref().unwrap()[SPEC_HASH_ANNOTATION],
            "f00d"
        );
        // 39 characters with a UUID, against the 63 an RFC 1035 name allows.
        assert!(name.len() <= 63, "{name}");
    }

    /// A Deployment's selector is immutable, and its pods have to match it, or the ReplicaSet
    /// adopts nothing and the Service selects nothing.
    #[test]
    fn the_pods_match_the_selector() {
        let spec = spec();
        let deployment = build(&spec, &site());
        let selector = deployment
            .spec
            .as_ref()
            .unwrap()
            .selector
            .match_labels
            .clone()
            .expect("a selector");
        let pod_labels = deployment
            .spec
            .as_ref()
            .unwrap()
            .template
            .metadata
            .as_ref()
            .unwrap()
            .labels
            .clone()
            .expect("pod labels");

        assert!(!selector.is_empty());
        for (key, value) in &selector {
            assert_eq!(pod_labels.get(key), Some(value));
        }
    }

    /// Pahoa's `flock` means two pods cannot overlap, so a rolling update cannot work.
    #[test]
    fn the_strategy_is_recreate_with_no_rolling_update_block() {
        let deployment = build(&spec(), &site());
        let strategy = deployment.spec.as_ref().unwrap().strategy.clone().unwrap();
        assert_eq!(strategy.type_.as_deref(), Some("Recreate"));
        assert!(
            strategy.rolling_update.is_none(),
            "a rollingUpdate block alongside Recreate is rejected by the API server"
        );
        assert_eq!(deployment.spec.as_ref().unwrap().replicas, Some(1));
    }

    /// The whole point of `envFrom`: the manifest names a Secret and carries no value.
    #[test]
    fn no_credential_appears_anywhere_in_the_manifest() {
        let spec = spec();
        let deployment = build(&spec, &site());
        let container = pahoa(&deployment);

        let secret_ref = container.env_from.as_ref().unwrap()[0]
            .secret_ref
            .as_ref()
            .unwrap();
        assert_eq!(secret_ref.name, object_name(spec.room_id));

        // Every plain `env` entry is a downward-API reference, never a literal.
        for var in container.env.as_ref().unwrap() {
            assert!(var.value.is_none(), "{} carries a literal value", var.name);
            assert!(var.value_from.is_some(), "{}", var.name);
        }

        let rendered = serde_json::to_string(&deployment).expect("serializes");
        for word in [
            "PAHOA_ADMIN_TOKEN",
            "PAHOA_PASSWORD",
            "PAHOA_SLOT_PASSWORDS",
        ] {
            assert!(!rendered.contains(word), "{word} reached the pod spec");
        }
    }

    /// The banner's `pod`, `namespace` and `node`, which Kubernetes will not provide unasked.
    #[test]
    fn the_downward_api_names_the_three_fields_pahoa_reads() {
        let deployment = build(&spec(), &site());
        let vars: Vec<(String, String)> = pahoa(&deployment)
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v.name.clone(),
                    v.value_from
                        .as_ref()
                        .unwrap()
                        .field_ref
                        .as_ref()
                        .unwrap()
                        .field_path
                        .clone(),
                )
            })
            .collect();

        assert_eq!(
            vars,
            [
                ("POD_NAME".to_string(), "metadata.name".to_string()),
                (
                    "POD_NAMESPACE".to_string(),
                    "metadata.namespace".to_string()
                ),
                ("NODE_NAME".to_string(), "spec.nodeName".to_string()),
            ]
        );
    }

    #[test]
    fn both_ports_are_declared_and_the_filtered_one_is_optional() {
        let mut spec = spec();
        let deployment = build(&spec, &site());
        let ports = pahoa(&deployment).ports.clone().unwrap();
        assert_eq!(
            ports
                .iter()
                .map(|p| (p.name.clone().unwrap(), p.container_port))
                .collect::<Vec<_>>(),
            [
                (PORT_FULL.to_string(), 40000),
                (PORT_FILTERED.to_string(), 40001)
            ]
        );

        spec.wants_filtered = false;
        let deployment = build(&spec, &site());
        let ports = pahoa(&deployment).ports.clone().unwrap();
        assert_eq!(ports.len(), 1, "only the full feed");
        // The probes still have a port to dial, which is why they name the full feed.
        assert_eq!(ports[0].name.as_deref(), Some(PORT_FULL));
    }

    /// HTTPS on the game port, by port *name* so a reallocation cannot leave a probe behind.
    #[test]
    fn both_probes_are_https_on_the_named_game_port() {
        let deployment = build(&spec(), &site());
        let container = pahoa(&deployment);

        for probe in [
            container.startup_probe.as_ref().unwrap(),
            container.readiness_probe.as_ref().unwrap(),
        ] {
            let get = probe.http_get.as_ref().unwrap();
            assert_eq!(get.path.as_deref(), Some("/healthz"));
            assert_eq!(get.scheme.as_deref(), Some("HTTPS"));
            assert_eq!(get.port, IntOrString::String(PORT_FULL.to_string()));
        }

        // Five minutes for the first success, fifteen seconds to be declared unready later: a cold
        // start is an image pull plus a save restored from CephFS, and after that it is a live room.
        let startup = container.startup_probe.as_ref().unwrap();
        assert_eq!(
            startup.period_seconds.unwrap() * startup.failure_threshold.unwrap(),
            300
        );
        assert_eq!(
            container
                .readiness_probe
                .as_ref()
                .unwrap()
                .failure_threshold,
            Some(3)
        );
    }

    /// Both limits are mandatory, for different reasons, and neither is a guess.
    #[test]
    fn the_cpu_limit_is_present_and_the_memory_follows_the_formula() {
        let spec = spec();
        let resources = pahoa(&build(&spec, &site())).resources.clone().unwrap();
        let requests = resources.requests.unwrap();
        let limits = resources.limits.unwrap();

        assert_eq!(limits["cpu"], Quantity("2".to_string()));
        // Steady state, not burst -- see the note on `resources`. The gap between the two is the
        // point: a room reserves little and is allowed to spike.
        assert_eq!(requests["cpu"], Quantity("50m".to_string()));

        assert_eq!(
            requests["memory"],
            quantity_bytes(crate::spec::room::memory_request_bytes(spec.slot_count))
        );
        assert_eq!(
            limits["memory"],
            quantity_bytes(crate::spec::room::memory_limit_bytes(spec.slot_count))
        );
        // A 96-slot room sits on the floor: 64 MiB of budget plus 192 MiB of overhead.
        assert_eq!(requests["memory"], Quantity("262144Ki".to_string()));
    }

    #[test]
    fn quantities_are_readable_where_that_is_exact_and_bytes_otherwise() {
        assert_eq!(quantity_bytes(64 * 1024 * 1024), Quantity("65536Ki".into()));
        assert_eq!(quantity_bytes(1025), Quantity("1025".into()));
    }

    /// A room mounts its own directory and the shared snapshot, and nothing else on a volume that
    /// also holds every other room's save and every generation.
    #[test]
    fn the_mounts_are_scoped_by_subpath_and_match_the_argv() {
        let spec = spec();
        let deployment = build(&spec, &site());
        let container = pahoa(&deployment);
        let mounts = container.volume_mounts.clone().unwrap();

        let state = mounts.iter().find(|m| m.mount_path == SAVE_DIR).unwrap();
        assert_eq!(
            state.sub_path.as_deref(),
            Some(&*format!("rooms/{}", spec.room_id))
        );
        assert_ne!(state.read_only, Some(true), "the room writes its own save");

        // No `/shared`: pahoa compiles its hint blacklist in and removed `--snapshot`, so there is
        // nothing for a room to read out of that subtree.
        assert!(
            !mounts.iter().any(|m| m.mount_path == "/shared"),
            "a room mounts nothing at /shared"
        );

        let tls = mounts.iter().find(|m| m.mount_path == TLS_DIR).unwrap();
        assert_eq!(tls.read_only, Some(true));

        // The argv and the mounts are one fact stated twice; this is where they meet.
        let argv = container.args.clone().unwrap();
        for mount in &mounts {
            assert!(
                argv.iter().any(|a| a.contains(&mount.mount_path)),
                "nothing in argv uses {}",
                mount.mount_path
            );
        }
    }

    #[test]
    fn the_certificate_is_readable_by_a_non_root_room() {
        let deployment = build(&spec(), &site());
        let pod = pod(&deployment);

        let tls = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "tls")
            .unwrap()
            .secret
            .clone()
            .unwrap();
        assert_eq!(tls.secret_name.as_deref(), Some("puna-room-tls"));
        // 0440 with fsGroup 1000: root:1000, group-readable. 0400 would be root-only, and the room
        // is uid 1000.
        assert_eq!(tls.default_mode, Some(0o440));
        assert_eq!(pod.security_context.as_ref().unwrap().fs_group, Some(1000));
    }

    /// The mechanical half of the tier split, plus the hardening that costs nothing here.
    #[test]
    fn a_room_pod_has_no_api_access_and_no_writable_root() {
        let deployment = build(&spec(), &site());
        let pod = pod(&deployment);

        assert_eq!(pod.service_account_name.as_deref(), Some("puna-room"));
        assert_eq!(pod.automount_service_account_token, Some(false));
        // Hundreds of Services in this namespace, each of which would otherwise inject a pair of
        // environment variables into every room.
        assert_eq!(pod.enable_service_links, Some(false));
        assert_eq!(pod.termination_grace_period_seconds, Some(45));

        let security = pahoa(&deployment).security_context.clone().unwrap();
        assert_eq!(security.allow_privilege_escalation, Some(false));
        assert_eq!(security.read_only_root_filesystem, Some(true));
        assert_eq!(
            security.capabilities.unwrap().drop,
            Some(vec!["ALL".to_string()])
        );

        let pod_security = pod.security_context.clone().unwrap();
        assert_eq!(pod_security.run_as_non_root, Some(true));
        assert_eq!(
            pod_security.seccomp_profile.unwrap().type_,
            "RuntimeDefault"
        );
    }

    /// Hundreds of rooms, and a room's state is on the volume rather than in a ReplicaSet.
    #[test]
    fn no_replicaset_history_is_kept() {
        let deployment = build(&spec(), &site());
        assert_eq!(
            deployment.spec.as_ref().unwrap().revision_history_limit,
            Some(0)
        );
        assert_eq!(
            deployment.spec.as_ref().unwrap().progress_deadline_seconds,
            Some(300)
        );
    }

    /// Same inputs, same manifest: the applier compares hashes, not objects, but a builder that
    /// varied would make every comparison downstream suspect.
    #[test]
    fn the_rendering_is_deterministic() {
        let spec = spec();
        let a = serde_json::to_string(&build(&spec, &site())).unwrap();
        let b = serde_json::to_string(&build(&spec, &site())).unwrap();
        assert_eq!(a, b);
    }
}
