//! An in-memory cluster.
//!
//! The plan calls the suite built on this "the highest-value suite in the project", and the reason
//! is that the room lifecycle is almost entirely *ordering*: the Secret before the pod, the uid
//! before the ownership, the ingress address before the room is advertised, the Deployment gone
//! before the directory moves. None of that needs a cluster to be wrong in, and all of it is
//! expensive to discover in one.
//!
//! ## What it models, and where the fidelity actually matters
//!
//! **Garbage collection by uid.** Deleting a Deployment removes every Service and Secret whose
//! `owner_uid` matches its uid — and **leaves the rest behind**. That asymmetry is the whole point:
//! Puna's teardown is `delete Deployment` and nothing else, so an ownerReference written without
//! the right uid does not fail, it leaks. Here that leak is a surviving object in a test assertion
//! rather than a Service holding a port for a room that no longer exists.
//!
//! **`409 AlreadyExists` as a distinct outcome**, because §7 treats it as success and re-`get`s for
//! the uid. A fake that quietly overwrote would hide the one path a double-run takes.
//!
//! **Ingress addresses arrive late.** [`FakeCluster::delay_ingress`] withholds
//! `status.loadBalancer.ingress` for a few `get_service` calls, so the read-back poll is exercised
//! without a real timer; [`FakeCluster::set_ingress_ip`] hands out the *wrong* address, which is
//! the silent Cilium failure §5 quarantines a pair over.
//!
//! **Its own clock**, advanced by hand, so `created_at` is something a test states rather than
//! something it waits for.
//!
//! What it deliberately does not model: admission, finalizers, scheduling, or anything about pods.
//! Readiness is a value a test sets, because every question worth asking here is about what Puna
//! does with the answer.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use puna_core::ids::RoomId;

use super::{
    ClusterApi, ClusterError, OwnerRef, Result, RoomDeployment, RoomSecret, RoomService, RoomSpec,
    SecretSpec, ServiceSpec,
};

/// The address the fake's IPAM hands out, matching the one dev is configured with.
const DEFAULT_LB_IP: &str = "38.246.56.121";

/// One call, as the log records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    ListDeployments,
    ListServices,
    ListSecrets,
    GetDeployment,
    CreateDeployment,
    DeleteDeployment,
    ApplySecret,
    DeleteSecret,
    CreateService,
    GetService,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub op: Op,
    pub name: String,
    /// Whether an ownerReference was set. Only meaningful for [`Op::ApplySecret`], where the
    /// answer changes between the two applies §7 makes on purpose.
    pub owned: bool,
}

#[derive(Debug, Clone)]
struct StoredDeployment {
    spec: RoomSpec,
    uid: String,
    replicas: i32,
    ready_replicas: i32,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct StoredService {
    spec: ServiceSpec,
    /// Counts down on each `get_service`; the address appears when it reaches zero.
    pending_reads: u32,
}

#[derive(Default)]
struct Inner {
    deployments: BTreeMap<String, StoredDeployment>,
    services: BTreeMap<String, StoredService>,
    secrets: BTreeMap<String, SecretSpec>,
    calls: Vec<Call>,
    next_uid: u64,
    failures: Vec<(Op, ClusterError)>,
    /// Names the next `get_deployment` will claim not to see, one shot each.
    withheld: Vec<String>,
    ingress_ip: String,
    ingress_delay: u32,
    now: DateTime<Utc>,
}

pub struct FakeCluster {
    inner: Mutex<Inner>,
}

impl FakeCluster {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                ingress_ip: DEFAULT_LB_IP.to_string(),
                now: DateTime::from_timestamp(1_770_000_000, 0).expect("a valid fixed instant"),
                ..Default::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("the fake is never poisoned")
    }

    /// Record a call, and hand back an injected failure if one is queued for it.
    fn record(&self, op: Op, name: &str, owned: bool) -> Result<()> {
        let mut inner = self.lock();
        inner.calls.push(Call {
            op,
            name: name.to_string(),
            owned,
        });
        if let Some(index) = inner.failures.iter().position(|(queued, _)| *queued == op) {
            return Err(inner.failures.remove(index).1);
        }
        Ok(())
    }

    // -- the test-facing controls --

    /// Fail the next call to `op` with `error`, once.
    pub fn fail_next(&self, op: Op, error: ClusterError) {
        self.lock().failures.push((op, error));
    }

    /// Move the fake's clock, which is what `created_at` is stamped from.
    pub fn advance(&self, by: Duration) {
        let mut inner = self.lock();
        inner.now += by;
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.lock().now
    }

    /// Give a room's Deployment a ready replica, as a scheduled pod passing its probe would.
    pub fn set_ready(&self, room: RoomId, ready: bool) {
        let name = super::object_name(room);
        let mut inner = self.lock();
        if let Some(deployment) = inner.deployments.get_mut(&name) {
            deployment.replicas = 1;
            deployment.ready_replicas = i32::from(ready);
        }
    }

    /// Hand out a different address than the one asked for.
    ///
    /// The silent Cilium failure: sharing degrades, a second IP is allocated, and the room comes up
    /// perfectly healthy on an address DNS never mentions.
    pub fn set_ingress_ip(&self, ip: &str) {
        self.lock().ingress_ip = ip.to_string();
    }

    /// Withhold the ingress address for the next `reads` calls to `get_service`.
    pub fn delay_ingress(&self, reads: u32) {
        self.lock().ingress_delay = reads;
    }

    /// Make the next `get_deployment` for this room answer `None` though the object exists.
    ///
    /// The stale read, modelled: lists and gets go to the watch cache at `resourceVersion=0`, so a
    /// Deployment created moments ago can be missing from one. It is the only way to reach the `409`
    /// branch in the applier, and the reason that branch treats a conflict as success.
    pub fn withhold_deployment(&self, room: RoomId) {
        let name = super::object_name(room);
        self.lock().withheld.push(name);
    }

    /// Delete a Deployment the way an operator with `kubectl` would: it cascades, and Puna is not
    /// told. Not recorded in the call log, because Puna did not make the call.
    pub fn hand_delete_deployment(&self, room: RoomId) {
        let name = super::object_name(room);
        let mut inner = self.lock();
        collect_dependents(&mut inner, &name);
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// Just the sequence, which is what an ordering assertion is usually about.
    pub fn ops(&self) -> Vec<Op> {
        self.lock().calls.iter().map(|c| c.op).collect()
    }

    pub fn clear_calls(&self) {
        self.lock().calls.clear();
    }

    /// Names of everything the fake is holding, so a teardown test can assert on emptiness without
    /// three separate reads.
    pub fn object_names(&self) -> Vec<String> {
        let inner = self.lock();
        inner
            .deployments
            .keys()
            .chain(inner.services.keys())
            .chain(inner.secrets.keys())
            .cloned()
            .collect()
    }
}

impl Default for FakeCluster {
    fn default() -> Self {
        Self::new()
    }
}

/// Remove a Deployment and everything the garbage collector would take with it.
///
/// **Matching on uid, not on name.** A dependent whose `owner_uid` is stale — from a Deployment
/// that was recreated, or from an ownerReference written before the uid was known — survives,
/// exactly as it would in the cluster. That is the leak, and it is worth being able to see.
fn collect_dependents(inner: &mut Inner, name: &str) {
    let Some(deployment) = inner.deployments.remove(name) else {
        return;
    };
    let uid = deployment.uid;
    inner
        .services
        .retain(|_, service| service.spec.owner.uid != uid);
    inner
        .secrets
        .retain(|_, secret| secret.owner.as_ref().map(|o| &o.uid) != Some(&uid));
}

fn read_deployment(stored: &StoredDeployment) -> RoomDeployment {
    RoomDeployment {
        name: stored.spec.name(),
        uid: stored.uid.clone(),
        room_id: Some(stored.spec.room_id),
        spec_hash: Some(stored.spec.spec_hash.clone()),
        // The fake stores the spec it was created with, so the image it reports is the image it was
        // asked for -- which is what makes a test that changes the image and expects the observed
        // value to follow meaningful.
        image: Some(stored.spec.image.clone()),
        replicas: stored.replicas,
        ready_replicas: stored.ready_replicas,
        created_at: stored.created_at,
    }
}

fn read_secret(spec: &SecretSpec) -> RoomSecret {
    RoomSecret {
        name: spec.name(),
        room_id: Some(spec.room_id),
        owner_uid: spec.owner.as_ref().map(|o| o.uid.clone()),
    }
}

#[async_trait]
impl ClusterApi for FakeCluster {
    async fn list_deployments(&self) -> Result<Vec<RoomDeployment>> {
        self.record(Op::ListDeployments, "", false)?;
        Ok(self
            .lock()
            .deployments
            .values()
            .map(read_deployment)
            .collect())
    }

    async fn list_services(&self) -> Result<Vec<RoomService>> {
        self.record(Op::ListServices, "", false)?;
        let inner = self.lock();
        Ok(inner
            .services
            .values()
            .map(|stored| RoomService {
                name: stored.spec.name(),
                room_id: Some(stored.spec.room_id),
                // A list does not consume the delay: only the read-back poll does, and charging a
                // sweep for it would make the two interfere.
                ingress_ip: (stored.pending_reads == 0).then(|| inner.ingress_ip.clone()),
                owner_uid: Some(stored.spec.owner.uid.clone()),
            })
            .collect())
    }

    async fn list_secrets(&self) -> Result<Vec<RoomSecret>> {
        self.record(Op::ListSecrets, "", false)?;
        Ok(self.lock().secrets.values().map(read_secret).collect())
    }

    async fn get_deployment(&self, name: &str) -> Result<Option<RoomDeployment>> {
        self.record(Op::GetDeployment, name, false)?;
        let mut inner = self.lock();
        if let Some(index) = inner.withheld.iter().position(|held| held == name) {
            inner.withheld.remove(index);
            return Ok(None);
        }
        Ok(inner.deployments.get(name).map(read_deployment))
    }

    async fn create_deployment(&self, spec: &RoomSpec) -> Result<String> {
        let name = spec.name();
        self.record(Op::CreateDeployment, &name, false)?;

        let mut inner = self.lock();
        if inner.deployments.contains_key(&name) {
            return Err(ClusterError::AlreadyExists { name });
        }

        inner.next_uid += 1;
        let uid = format!("uid-{}", inner.next_uid);
        let created_at = inner.now;
        inner.deployments.insert(
            name,
            StoredDeployment {
                spec: spec.clone(),
                uid: uid.clone(),
                // A fresh Deployment has no ready replica: a test says when the pod came up,
                // because every question here is about what Puna does while it has not.
                replicas: 1,
                ready_replicas: 0,
                created_at,
            },
        );
        Ok(uid)
    }

    async fn delete_deployment(&self, name: &str) -> Result<()> {
        self.record(Op::DeleteDeployment, name, false)?;
        let mut inner = self.lock();
        // Absent is success: the caller wanted it gone and it is. Teardown runs every tick until
        // the room's row is gone, so anything else would turn a completed delete into an error.
        collect_dependents(&mut inner, name);
        Ok(())
    }

    async fn apply_secret(&self, spec: &SecretSpec) -> Result<()> {
        let name = spec.name();
        self.record(Op::ApplySecret, &name, spec.owner.is_some())?;
        // Server-side apply: repeating it is free, and the second apply is how ownership arrives.
        self.lock().secrets.insert(name, spec.clone());
        Ok(())
    }

    async fn delete_secret(&self, name: &str) -> Result<()> {
        self.record(Op::DeleteSecret, name, false)?;
        self.lock().secrets.remove(name);
        Ok(())
    }

    async fn create_service(&self, spec: &ServiceSpec) -> Result<()> {
        let name = spec.name();
        self.record(Op::CreateService, &name, true)?;

        let mut inner = self.lock();
        if inner.services.contains_key(&name) {
            return Err(ClusterError::AlreadyExists { name });
        }
        let pending_reads = inner.ingress_delay;
        inner.services.insert(
            name,
            StoredService {
                spec: spec.clone(),
                pending_reads,
            },
        );
        Ok(())
    }

    async fn get_service(&self, name: &str) -> Result<Option<RoomService>> {
        self.record(Op::GetService, name, false)?;

        let mut inner = self.lock();
        let ingress_ip = inner.ingress_ip.clone();
        let Some(stored) = inner.services.get_mut(name) else {
            return Ok(None);
        };
        // Decided before the decrement, so `delay_ingress(n)` withholds the address from exactly
        // `n` reads rather than `n - 1`.
        let answered = stored.pending_reads == 0;
        if !answered {
            stored.pending_reads -= 1;
        }
        Ok(Some(RoomService {
            name: stored.spec.name(),
            room_id: Some(stored.spec.room_id),
            ingress_ip: answered.then_some(ingress_ip),
            owner_uid: Some(stored.spec.owner.uid.clone()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room_spec(room: RoomId, hash: &str) -> RoomSpec {
        RoomSpec {
            room_id: room,
            spec_hash: hash.to_string(),
            image: "pahoa:test".into(),
            base_port: 40000,
            wants_filtered: true,
            slot_count: 4,
            save_interval_secs: 30,
            use_embedded_options: true,
        }
    }

    fn secret_spec(room: RoomId, owner: Option<OwnerRef>) -> SecretSpec {
        let mut data = crate::spec::secret::SecretData::new();
        data.insert("PAHOA_ADMIN_TOKEN".into(), "a".repeat(52));
        SecretSpec {
            room_id: room,
            data,
            owner,
        }
    }

    fn service_spec(room: RoomId, owner: OwnerRef) -> ServiceSpec {
        ServiceSpec {
            room_id: room,
            base_port: 40000,
            wants_filtered: true,
            owner,
        }
    }

    fn owner(room: RoomId, uid: &str) -> OwnerRef {
        OwnerRef {
            name: super::super::object_name(room),
            uid: uid.to_string(),
        }
    }

    /// §7's ordering, walked once: Secret unowned, Deployment, Secret owned, Service.
    #[tokio::test]
    async fn the_provisioning_sequence_leaves_one_owned_object_of_each_kind() {
        let cluster = FakeCluster::new();
        let room = RoomId::new();

        cluster
            .apply_secret(&secret_spec(room, None))
            .await
            .expect("first apply");
        let uid = cluster
            .create_deployment(&room_spec(room, "hash-1"))
            .await
            .expect("create");
        cluster
            .apply_secret(&secret_spec(room, Some(owner(room, &uid))))
            .await
            .expect("second apply");
        cluster
            .create_service(&service_spec(room, owner(room, &uid)))
            .await
            .expect("service");

        assert_eq!(
            cluster.ops(),
            [
                Op::ApplySecret,
                Op::CreateDeployment,
                Op::ApplySecret,
                Op::CreateService,
            ]
        );
        // The Secret exists before the pod could start, and is owned afterwards. Both applies are
        // the same object; the difference between them is the whole reason there are two.
        let applies: Vec<bool> = cluster
            .calls()
            .into_iter()
            .filter(|c| c.op == Op::ApplySecret)
            .map(|c| c.owned)
            .collect();
        assert_eq!(applies, [false, true]);

        let snapshot = cluster.snapshot().await.expect("snapshot");
        assert_eq!(snapshot.deployment(room).map(|d| d.uid.clone()), Some(uid));
        assert_eq!(
            snapshot.secret(room).and_then(|s| s.owner_uid.clone()),
            snapshot.deployment(room).map(|d| d.uid.clone()),
            "the Secret must end up owned by the Deployment that was actually created"
        );
        assert!(snapshot.service(room).is_some());
    }

    /// Deleting the Deployment is the whole of teardown, and it has to be enough.
    #[tokio::test]
    async fn deleting_the_deployment_collects_everything_it_owns() {
        let cluster = FakeCluster::new();
        let room = RoomId::new();

        let uid = cluster
            .create_deployment(&room_spec(room, "hash-1"))
            .await
            .expect("create");
        cluster
            .apply_secret(&secret_spec(room, Some(owner(room, &uid))))
            .await
            .expect("apply");
        cluster
            .create_service(&service_spec(room, owner(room, &uid)))
            .await
            .expect("service");

        cluster
            .delete_deployment(&super::super::object_name(room))
            .await
            .expect("delete");

        assert!(
            cluster.object_names().is_empty(),
            "still holding {:?}",
            cluster.object_names()
        );
    }

    /// The failure this fake exists to make visible.
    ///
    /// An ownerReference carrying a uid that does not match the Deployment is not an error
    /// anywhere: the object is created, the room runs, and teardown silently leaves it behind
    /// holding a port. In the cluster that is discovered weeks later; here it is an assertion.
    #[tokio::test]
    async fn an_object_owned_by_the_wrong_uid_survives_teardown() {
        let cluster = FakeCluster::new();
        let room = RoomId::new();

        let uid = cluster
            .create_deployment(&room_spec(room, "hash-1"))
            .await
            .expect("create");
        cluster
            .create_service(&service_spec(
                room,
                owner(room, "uid-from-a-previous-deployment"),
            ))
            .await
            .expect("service");

        cluster
            .delete_deployment(&super::super::object_name(room))
            .await
            .expect("delete");

        assert_ne!(uid, "uid-from-a-previous-deployment");
        assert_eq!(
            cluster.object_names(),
            [super::super::object_name(room)],
            "the Service outlives the room, which is what a dropped or stale uid buys"
        );
        // ...and the sweep can see it, because a Service reports its owner.
        let orphan = cluster.list_services().await.expect("list");
        assert_eq!(
            orphan[0].owner_uid.as_deref(),
            Some("uid-from-a-previous-deployment")
        );
    }

    /// §7 treats `409` as success and re-`get`s for the uid, so it has to be distinguishable.
    #[tokio::test]
    async fn creating_twice_reports_already_exists_and_keeps_the_first_uid() {
        let cluster = FakeCluster::new();
        let room = RoomId::new();
        let name = super::super::object_name(room);

        let uid = cluster
            .create_deployment(&room_spec(room, "hash-1"))
            .await
            .expect("create");
        let err = cluster
            .create_deployment(&room_spec(room, "hash-1"))
            .await
            .expect_err("the second create must not overwrite");
        assert_eq!(err, ClusterError::AlreadyExists { name: name.clone() });
        assert!(!err.is_fatal(), "409 is success, so it cannot be fatal");

        // The recovery path: re-get, and the uid is still the one to persist.
        assert_eq!(
            cluster
                .get_deployment(&name)
                .await
                .expect("get")
                .map(|d| d.uid),
            Some(uid)
        );
        // A Service created before its Deployment's uid is known is the same shape of mistake.
        let uid = cluster
            .get_deployment(&name)
            .await
            .expect("get")
            .unwrap()
            .uid;
        cluster
            .create_service(&service_spec(room, owner(room, &uid)))
            .await
            .expect("service");
        assert_eq!(
            cluster
                .create_service(&service_spec(room, owner(room, &uid)))
                .await
                .expect_err("second create"),
            ClusterError::AlreadyExists { name }
        );
    }

    /// Deleting something that is already gone is what a repeated teardown does every tick.
    #[tokio::test]
    async fn deleting_what_is_not_there_is_success() {
        let cluster = FakeCluster::new();
        let room = RoomId::new();
        let name = super::super::object_name(room);

        cluster.delete_deployment(&name).await.expect("deployment");
        cluster.delete_secret(&name).await.expect("secret");
        assert_eq!(cluster.get_deployment(&name).await.expect("get"), None);
        assert_eq!(cluster.get_service(&name).await.expect("get"), None);
    }

    #[tokio::test]
    async fn readiness_and_the_clock_are_things_a_test_states() {
        let cluster = FakeCluster::new();
        let room = RoomId::new();
        let started = cluster.now();

        cluster.advance(Duration::minutes(5));
        cluster
            .create_deployment(&room_spec(room, "hash-1"))
            .await
            .expect("create");

        let deployment = cluster.snapshot().await.expect("snapshot");
        let deployment = deployment.deployment(room).expect("the deployment").clone();
        assert_eq!(deployment.created_at, started + Duration::minutes(5));
        assert_eq!(
            (deployment.replicas, deployment.ready_replicas),
            (1, 0),
            "a Deployment that was just created has no ready replica yet"
        );

        cluster.set_ready(room, true);
        let ready = cluster.list_deployments().await.expect("list");
        assert_eq!(ready[0].ready_replicas, 1);

        cluster.set_ready(room, false);
        let not_ready = cluster.list_deployments().await.expect("list");
        assert_eq!(not_ready[0].ready_replicas, 0);
    }

    /// The read-back poll: IPAM answers in 0.3-0.5s, so the first look usually finds nothing.
    #[tokio::test]
    async fn an_ingress_address_can_arrive_late_or_wrong() {
        let cluster = FakeCluster::new();
        let room = RoomId::new();
        let name = super::super::object_name(room);

        cluster.delay_ingress(2);
        cluster
            .create_service(&service_spec(room, owner(room, "uid-1")))
            .await
            .expect("service");

        for attempt in 0..2 {
            let service = cluster.get_service(&name).await.expect("get").unwrap();
            assert_eq!(service.ingress_ip, None, "attempt {attempt}");
        }
        assert_eq!(
            cluster
                .get_service(&name)
                .await
                .expect("get")
                .unwrap()
                .ingress_ip
                .as_deref(),
            Some(DEFAULT_LB_IP)
        );

        // Sharing degraded: healthy, allocated, and on an address DNS never mentions.
        let other = RoomId::new();
        cluster.delay_ingress(0);
        cluster.set_ingress_ip("38.246.56.122");
        cluster
            .create_service(&service_spec(other, owner(other, "uid-2")))
            .await
            .expect("service");
        let wrong = cluster
            .get_service(&super::super::object_name(other))
            .await
            .expect("get")
            .unwrap();
        assert_eq!(wrong.ingress_ip.as_deref(), Some("38.246.56.122"));
    }

    #[tokio::test]
    async fn injected_failures_fire_once_for_the_call_they_name() {
        let cluster = FakeCluster::new();
        let room = RoomId::new();

        cluster.fail_next(
            Op::CreateDeployment,
            ClusterError::Transient("apiserver said 503".into()),
        );
        let err = cluster
            .create_deployment(&room_spec(room, "hash-1"))
            .await
            .expect_err("injected");
        assert_eq!(err, ClusterError::Transient("apiserver said 503".into()));
        assert!(!err.is_fatal(), "the tick is the retry");

        // Once, so the next tick's attempt is the one that succeeds -- which is what
        // level-triggered recovery looks like.
        cluster
            .create_deployment(&room_spec(room, "hash-1"))
            .await
            .expect("the retry");

        cluster.fail_next(
            Op::ApplySecret,
            ClusterError::Fatal("secrets is forbidden".into()),
        );
        let err = cluster
            .apply_secret(&secret_spec(room, None))
            .await
            .expect_err("injected");
        assert!(
            err.is_fatal(),
            "RBAC cannot be fixed by trying again in 30 seconds"
        );
        // The call is still logged: a failed attempt is a thing that happened.
        assert!(cluster.calls().iter().any(|c| c.op == Op::ApplySecret));
    }

    /// A Deployment removed by hand is the case the sweep exists for.
    #[tokio::test]
    async fn a_hand_deleted_deployment_cascades_and_is_not_logged_as_punas_doing() {
        let cluster = FakeCluster::new();
        let room = RoomId::new();

        let uid = cluster
            .create_deployment(&room_spec(room, "hash-1"))
            .await
            .expect("create");
        cluster
            .create_service(&service_spec(room, owner(room, &uid)))
            .await
            .expect("service");
        cluster.clear_calls();

        cluster.hand_delete_deployment(room);

        assert!(cluster.object_names().is_empty());
        assert_eq!(
            cluster.ops(),
            [],
            "Puna made no call, which is exactly why the room's row still says running"
        );
        assert_eq!(cluster.snapshot().await.expect("snapshot").deployments, []);
    }
}
