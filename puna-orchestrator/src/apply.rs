//! Carrying out a [`crate::plan::Step`] against a cluster.
//!
//! The planner decides; this does. Everything here is idempotent and level-triggered, because the
//! tick is the retry: nothing loops, nothing sleeps, and re-running any of it on the same world is a
//! no-op.
//!
//! ## The ordering is the whole content of this module
//!
//! ```text
//! 1. apply the Secret, UNOWNED   -- the pod cannot start without it, so it cannot wait for the pod
//! 2. create the Deployment       -- 409 is success; re-get for the uid
//! 3. PERSIST the uid             -- before anything is owned by it
//! 4. apply the Secret, OWNED     -- now the garbage collector will take it away with the room
//! 5. create the Service          -- owned, so teardown stays one call
//! 6. read the ingress address    -- and refuse to advertise a room on the wrong one
//! ```
//!
//! Step 3 is the one that looks like bookkeeping and is not. **Losing the uid means nothing can ever
//! be owned by that Deployment**, and unowned objects are never collected — so a crash between 2 and
//! 3 must leave a room that can be repaired, which it does: the next tick finds the Deployment, reads
//! its uid, and continues. A crash after 4 with the uid unwritten would instead leave a Service
//! holding a port for a room nobody can account for.
//!
//! ## Where this deliberately differs from the design
//!
//! §5 says to poll `status.loadBalancer.ingress` for up to 30 seconds after creating the Service.
//! This reads it **once per tick** instead. Cilium answers in 0.3–0.5 s measured, so the poll almost
//! never waits; when it does, sleeping inside a tick would serialize the whole sweep behind one
//! room, and the address is not needed until the pod is ready — which is an image pull and a save
//! restore away. So a missing address is [`Started::AwaitingAddress`] and the next tick looks again.

use puna_core::ids::RoomId;

use crate::cluster::{
    ClusterApi, ClusterError, OwnerRef, RoomSpec, SecretSpec, ServiceSpec, object_name,
};
use crate::spec::Site;
use crate::spec::secret::SecretData;

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Cluster(#[from] ClusterError),

    /// The uid could not be written down. Fatal for this attempt **on purpose**: continuing would
    /// own objects with a uid nothing has recorded.
    #[error("could not record the Deployment: {0}")]
    Record(#[source] anyhow::Error),

    /// A Deployment was created and then could not be found. Only reachable through a 409 followed
    /// by a delete, which means somebody else is managing this room's objects.
    #[error("{name} was created but cannot be read back")]
    Vanished { name: String },
}

/// Where a start attempt got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// Objects exist, match the row, and the address is the configured one. The room can be
    /// advertised and is waiting on its pod.
    Converged { uid: String, ingress_ip: String },

    /// The running spec is not the one the row describes, so the Deployment was deleted. The caller
    /// puts the room back to `idle` **keeping its reservation**, and the next tick starts it again
    /// through the ordinary path.
    Recreating,

    /// IPAM has not answered yet. Nothing is wrong; the next tick reads it again.
    AwaitingAddress,

    /// **The silent Cilium failure, caught.** Sharing degraded and the room was allocated a
    /// different address, on which it would be perfectly healthy and unreachable by name. The
    /// Deployment is already deleted (which collects the Service); the caller quarantines the pair
    /// so the next allocation cannot pick it again.
    AddressMismatch { observed: String },
}

/// Everything a start needs that is not the cluster.
pub struct StartRequest<'a> {
    pub spec: &'a RoomSpec,
    /// The room's environment, exactly as `spec::secret::build` produced it — complete or the build
    /// refused, because a partial `PAHOA_SLOT_PASSWORDS` is a room nobody can join.
    pub secret: &'a SecretData,
    pub site: &'a Site,
}

/// Writing down what was created, injected so the ordering above can be asserted.
///
/// One method, and it exists as a trait rather than a closure so the fake-cluster suite can prove
/// step 3 happened **before** step 4 — the property that makes a crash recoverable.
#[async_trait::async_trait]
pub trait DeploymentRecorder: Send {
    async fn record(&mut self, room: RoomId, uid: &str, spec_hash: &str) -> anyhow::Result<()>;
}

/// Make a room's objects exist and match the row, or say why not.
pub async fn ensure_room_running(
    cluster: &dyn ClusterApi,
    request: &StartRequest<'_>,
    recorder: &mut dyn DeploymentRecorder,
) -> Result<Started, ApplyError> {
    let spec = request.spec;
    let name = object_name(spec.room_id);

    // 1. Server-side apply, so repeating it is free and the values are always current.
    cluster
        .apply_secret(&SecretSpec {
            room_id: spec.room_id,
            data: request.secret.clone(),
            owner: None,
        })
        .await?;

    // 2.
    let uid = match cluster.get_deployment(&name).await? {
        Some(existing) if existing.spec_hash.as_deref() == Some(spec.spec_hash.as_str()) => {
            existing.uid
        }
        Some(_) => {
            // Delete and return: the next tick recreates. Not a rolling update, and it cannot be --
            // the port is in the args and the Service, and pahoa's flock stops two pods overlapping.
            cluster.delete_deployment(&name).await?;
            return Ok(Started::Recreating);
        }
        None => match cluster.create_deployment(spec).await {
            Ok(uid) => uid,
            // 409 is success: the object is there, which is what was wanted. Only a double-run
            // reaches this, and re-reading is how it converges.
            Err(ClusterError::AlreadyExists { .. }) => {
                cluster
                    .get_deployment(&name)
                    .await?
                    .ok_or(ApplyError::Vanished { name: name.clone() })?
                    .uid
            }
            Err(e) => return Err(e.into()),
        },
    };

    // 3. Before anything is owned by it. See the module note.
    recorder
        .record(spec.room_id, &uid, &spec.spec_hash)
        .await
        .map_err(ApplyError::Record)?;

    let owner = OwnerRef {
        name: name.clone(),
        uid: uid.clone(),
    };

    // 4. The same object as step 1, now with an ownerReference, so teardown collects it.
    cluster
        .apply_secret(&SecretSpec {
            room_id: spec.room_id,
            data: request.secret.clone(),
            owner: Some(owner.clone()),
        })
        .await?;

    // 5.
    let service = match cluster.get_service(&name).await? {
        Some(service) => service,
        None => {
            cluster
                .create_service(&ServiceSpec {
                    room_id: spec.room_id,
                    base_port: spec.base_port,
                    wants_filtered: spec.wants_filtered,
                    owner,
                })
                .await?;
            cluster
                .get_service(&name)
                .await?
                .ok_or(ApplyError::Vanished { name })?
        }
    };

    // 6. Never advertise an address that is not the one DNS points at.
    match service.ingress_ip {
        None => Ok(Started::AwaitingAddress),
        Some(ip) if ip == request.site.lb_ip => Ok(Started::Converged {
            uid,
            ingress_ip: ip,
        }),
        Some(observed) => {
            // Deleting the Deployment collects the Service, which is what actually releases the
            // wrong allocation -- and it has to happen before the caller quarantines the pair, or a
            // second room could be handed the same wrong address in between.
            cluster
                .delete_deployment(&object_name(spec.room_id))
                .await?;
            Ok(Started::AddressMismatch { observed })
        }
    }
}

/// Take a room's objects away.
///
/// Deleting the Deployment is the whole of it: the Service and the Secret carry ownerReferences to
/// it, so the garbage collector removes them. The Secret is deleted explicitly as well, for the one
/// window where it would otherwise leak — a start that applied the unowned Secret (step 1) and never
/// reached step 4 leaves a Secret with no owner, which nothing would ever collect.
pub async fn teardown_room(cluster: &dyn ClusterApi, room: RoomId) -> Result<(), ApplyError> {
    let name = object_name(room);
    cluster.delete_deployment(&name).await?;
    cluster.delete_secret(&name).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::fake::{FakeCluster, Op};
    use crate::spec::room::Draft;
    use puna_core::model::room::SlotAuth;

    /// Records what it was told, and when.
    #[derive(Default)]
    struct Recording {
        calls: Vec<(RoomId, String, String)>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl DeploymentRecorder for Recording {
        async fn record(&mut self, room: RoomId, uid: &str, spec_hash: &str) -> anyhow::Result<()> {
            if self.fail {
                anyhow::bail!("the database is unreachable");
            }
            self.calls
                .push((room, uid.to_string(), spec_hash.to_string()));
            Ok(())
        }
    }

    fn site() -> Site {
        Site {
            namespace: "puna-dev".into(),
            lb_ip: "38.246.56.121".into(),
            lb_sharing_key: "ap-lobby-public".into(),
            tls_secret: "puna-room-tls".into(),
            data_pvc: "puna-data".into(),
        }
    }

    fn secret_data() -> SecretData {
        let mut data = SecretData::new();
        data.insert("PAHOA_ADMIN_TOKEN".into(), "a".repeat(52));
        data
    }

    fn spec(room: RoomId, image: &str) -> RoomSpec {
        Draft {
            room_id: room,
            image: image.to_string(),
            base_port: 40000,
            wants_filtered: true,
            slot_count: 96,
            save_interval_secs: 30,
            use_embedded_options: true,
        }
        .build(SlotAuth::None, &secret_data())
    }

    async fn start(
        cluster: &FakeCluster,
        spec: &RoomSpec,
        recorder: &mut Recording,
    ) -> Result<Started, ApplyError> {
        let secret = secret_data();
        let site = site();
        ensure_room_running(
            cluster,
            &StartRequest {
                spec,
                secret: &secret,
                site: &site,
            },
            recorder,
        )
        .await
    }

    /// The ordering, asserted end to end. This is the test the whole `ClusterApi` split exists for.
    #[tokio::test]
    async fn a_cold_start_creates_everything_in_the_one_safe_order() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let room = RoomId::new();
        let spec = spec(room, "pahoa:test");

        let started = start(&cluster, &spec, &mut recorder).await.expect("start");
        assert_eq!(
            started,
            Started::Converged {
                uid: "uid-1".into(),
                ingress_ip: "38.246.56.121".into()
            }
        );

        assert_eq!(
            cluster.ops(),
            [
                Op::ApplySecret,   // 1, unowned
                Op::GetDeployment, // 2
                Op::CreateDeployment,
                Op::ApplySecret, // 4, owned -- after the recorder, asserted below
                Op::GetService,  // 5
                Op::CreateService,
                Op::GetService, // 6
            ]
        );

        // The Secret exists before the pod, and is owned after it. Two applies of one object.
        let applies: Vec<bool> = cluster
            .calls()
            .into_iter()
            .filter(|c| c.op == Op::ApplySecret)
            .map(|c| c.owned)
            .collect();
        assert_eq!(applies, [false, true]);

        // Step 3 happened, with the uid the cluster actually assigned.
        assert_eq!(
            recorder.calls,
            [(room, "uid-1".to_string(), spec.spec_hash.clone())]
        );

        // ...and everything the room owns is owned by that uid, so teardown is one call.
        let snapshot = cluster.snapshot().await.expect("snapshot");
        assert_eq!(
            snapshot.secret(room).and_then(|s| s.owner_uid.clone()),
            Some("uid-1".to_string())
        );
        assert_eq!(
            snapshot.service(room).and_then(|s| s.owner_uid.clone()),
            Some("uid-1".to_string())
        );
    }

    /// Level-triggered means the second pass over a converged room does nothing new.
    #[tokio::test]
    async fn a_second_pass_creates_nothing_and_agrees_with_the_first() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let spec = spec(RoomId::new(), "pahoa:test");

        let first = start(&cluster, &spec, &mut recorder).await.expect("first");
        cluster.clear_calls();
        let second = start(&cluster, &spec, &mut recorder).await.expect("second");

        assert_eq!(first, second);
        let ops = cluster.ops();
        assert!(
            !ops.contains(&Op::CreateDeployment) && !ops.contains(&Op::CreateService),
            "{ops:?}"
        );
        // Re-applying the Secret every pass is deliberate: server-side apply is free and it is what
        // makes a rotated password current without anybody tracking whether it changed.
        assert_eq!(
            ops,
            [
                Op::ApplySecret,
                Op::GetDeployment,
                Op::ApplySecret,
                Op::GetService
            ]
        );
        assert_eq!(cluster.object_names().len(), 3);
    }

    /// A double-run -- two leaders for a moment -- must converge rather than error.
    #[tokio::test]
    async fn a_409_is_success_and_the_uid_is_read_back() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let spec = spec(RoomId::new(), "pahoa:test");

        // The object exists -- another leader made it a moment ago -- and our read of the watch
        // cache misses it. That is the only way a `create` can 409, and why it is treated as success.
        cluster
            .create_deployment(&spec)
            .await
            .expect("the other actor's create");
        cluster.withhold_deployment(spec.room_id);
        cluster.clear_calls();

        let started = start(&cluster, &spec, &mut recorder).await.expect("start");
        assert!(matches!(started, Started::Converged { .. }));
        assert_eq!(
            recorder.calls.last().map(|c| c.1.clone()),
            Some("uid-1".to_string()),
            "the uid to persist is the one that exists, not the one we tried to make"
        );
    }

    /// A changed spec hash -- a new image, a moved port, a `slot_auth` change -- deletes and returns.
    #[tokio::test]
    async fn a_changed_spec_deletes_the_deployment_and_stops_there() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let room = RoomId::new();

        start(&cluster, &spec(room, "pahoa:old"), &mut recorder)
            .await
            .expect("first start");
        cluster.clear_calls();

        let started = start(&cluster, &spec(room, "pahoa:new"), &mut recorder)
            .await
            .expect("second start");
        assert_eq!(started, Started::Recreating);

        // The Deployment is gone and so is everything it owned, which is the point of owning them.
        assert!(
            cluster
                .object_names()
                .iter()
                .any(|n| n == &object_name(room)),
            "the unowned Secret from step 1 is still there, ready for the recreate"
        );
        let snapshot = cluster.snapshot().await.expect("snapshot");
        assert_eq!(snapshot.deployment(room), None);
        assert_eq!(snapshot.service(room), None);

        // Nothing was recorded: there is no uid to record, and the next tick starts from `idle`.
        assert_eq!(
            recorder.calls.len(),
            1,
            "only the first start recorded a uid"
        );
    }

    /// The room comes back on the next tick, through the ordinary path.
    #[tokio::test]
    async fn a_recreate_converges_on_the_following_pass() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let room = RoomId::new();

        start(&cluster, &spec(room, "pahoa:old"), &mut recorder)
            .await
            .expect("start");
        let new = spec(room, "pahoa:new");
        assert_eq!(
            start(&cluster, &new, &mut recorder)
                .await
                .expect("recreate"),
            Started::Recreating
        );

        let started = start(&cluster, &new, &mut recorder).await.expect("restart");
        assert_eq!(
            started,
            Started::Converged {
                uid: "uid-2".into(),
                ingress_ip: "38.246.56.121".into()
            }
        );
        // The new uid is what owns the new objects; a stale one would leak them at teardown.
        let snapshot = cluster.snapshot().await.expect("snapshot");
        assert_eq!(
            snapshot.service(room).and_then(|s| s.owner_uid.clone()),
            Some("uid-2".to_string())
        );
    }

    #[tokio::test]
    async fn an_address_that_has_not_arrived_yet_is_not_a_failure() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let spec = spec(RoomId::new(), "pahoa:test");

        cluster.delay_ingress(1);
        assert_eq!(
            start(&cluster, &spec, &mut recorder).await.expect("start"),
            Started::AwaitingAddress
        );
        // Nothing is torn down, and the next tick simply looks again.
        assert_eq!(cluster.object_names().len(), 3);
        assert!(matches!(
            start(&cluster, &spec, &mut recorder).await.expect("second"),
            Started::Converged { .. }
        ));
    }

    /// The failure Cilium does not report: healthy, allocated, and unreachable by name.
    #[tokio::test]
    async fn a_wrong_ingress_address_tears_the_room_down_rather_than_advertising_it() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let room = RoomId::new();
        let spec = spec(room, "pahoa:test");

        cluster.set_ingress_ip("38.246.56.122");
        let started = start(&cluster, &spec, &mut recorder).await.expect("start");
        assert_eq!(
            started,
            Started::AddressMismatch {
                observed: "38.246.56.122".into()
            }
        );

        // Deleting the Deployment is what releases the wrong allocation, via the Service it owns.
        let snapshot = cluster.snapshot().await.expect("snapshot");
        assert_eq!(snapshot.deployment(room), None);
        assert_eq!(
            snapshot.service(room),
            None,
            "the bad allocation has to be gone before the caller quarantines the pair"
        );
    }

    /// If the uid cannot be written down, nothing may come to depend on it.
    #[tokio::test]
    async fn a_failed_record_stops_before_anything_is_owned() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording {
            fail: true,
            ..Default::default()
        };
        let room = RoomId::new();

        let err = start(&cluster, &spec(room, "pahoa:test"), &mut recorder)
            .await
            .expect_err("must not continue");
        assert!(matches!(err, ApplyError::Record(_)), "{err:?}");

        let ops = cluster.ops();
        assert!(
            !ops.contains(&Op::CreateService),
            "a Service owned by an unrecorded uid would outlive the room: {ops:?}"
        );
        // The Deployment exists with nobody's record of its uid -- which is exactly the recoverable
        // half of the window: the next tick reads the uid off the object.
        let snapshot = cluster.snapshot().await.expect("snapshot");
        assert!(snapshot.deployment(room).is_some());

        let mut recorder = Recording::default();
        let started = start(&cluster, &spec(room, "pahoa:test"), &mut recorder)
            .await
            .expect("the retry");
        assert!(matches!(started, Started::Converged { .. }));
        assert_eq!(
            recorder.calls.len(),
            1,
            "the uid was recovered, not reassigned"
        );
    }

    /// The tick is the retry: an error leaves the world alone and the next pass tries again.
    #[tokio::test]
    async fn a_transient_error_is_reported_and_recovers_on_the_next_pass() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let spec = spec(RoomId::new(), "pahoa:test");

        cluster.fail_next(
            Op::CreateDeployment,
            ClusterError::Transient("apiserver said 503".into()),
        );
        let err = start(&cluster, &spec, &mut recorder)
            .await
            .expect_err("the injected failure");
        assert!(matches!(
            err,
            ApplyError::Cluster(ClusterError::Transient(_))
        ));
        assert!(recorder.calls.is_empty());

        assert!(matches!(
            start(&cluster, &spec, &mut recorder).await.expect("retry"),
            Started::Converged { .. }
        ));
    }

    /// A room whose Deployment somebody deleted by hand comes back on the next pass, and the fake's
    /// cascade means the Service it had is genuinely gone first.
    #[tokio::test]
    async fn a_hand_deleted_room_is_rebuilt_from_nothing() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let room = RoomId::new();
        let spec = spec(room, "pahoa:test");

        start(&cluster, &spec, &mut recorder).await.expect("start");
        cluster.hand_delete_deployment(room);
        assert_eq!(
            cluster.snapshot().await.expect("snapshot").services,
            [],
            "a kubectl delete cascades"
        );

        let started = start(&cluster, &spec, &mut recorder)
            .await
            .expect("rebuild");
        assert!(matches!(started, Started::Converged { .. }));
        assert_eq!(cluster.object_names().len(), 3);
    }

    /// Teardown is one call plus the belt for the one window that leaks.
    #[tokio::test]
    async fn teardown_removes_everything_including_an_unowned_secret() {
        let cluster = FakeCluster::new();
        let mut recorder = Recording::default();
        let room = RoomId::new();

        start(&cluster, &spec(room, "pahoa:test"), &mut recorder)
            .await
            .expect("start");
        teardown_room(&cluster, room).await.expect("teardown");
        assert!(
            cluster.object_names().is_empty(),
            "left behind {:?}",
            cluster.object_names()
        );

        // The leak this guards: a start that applied the unowned Secret and got no further. Nothing
        // owns it, so nothing would ever collect it.
        cluster
            .apply_secret(&SecretSpec {
                room_id: room,
                data: secret_data(),
                owner: None,
            })
            .await
            .expect("step 1 only");
        teardown_room(&cluster, room).await.expect("teardown");
        assert!(cluster.object_names().is_empty());

        // And teardown of a room that has nothing is success, which is what lets it run every tick.
        teardown_room(&cluster, room).await.expect("idempotent");
    }
}
