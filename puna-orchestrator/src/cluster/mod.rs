//! The Kubernetes boundary, as a trait.
//!
//! Everything the orchestrator does to a cluster goes through [`ClusterApi`], so the state machine
//! and the applier can be driven end to end with no cluster at all. That is what
//! [`fake::FakeCluster`] is for, and it is why this trait is narrow: ten methods, none of them
//! generic, all of them speaking Puna's own types.
//!
//! ## No `k8s_openapi` type crosses this boundary, in either direction
//!
//! The plan specified the read side that way already — [`RoomDeployment`] carries seven fields
//! rather than a whole `Deployment`, so [`crate::plan`] stays pure. **This module extends the same
//! rule to the write side**, which the plan drafted with `k8s_openapi` types: [`RoomSpec`],
//! [`ServiceSpec`] and [`SecretSpec`] describe a room's objects in Puna's terms, and rendering
//! them into manifests belongs to the one implementation that talks to a real API server.
//!
//! Three reasons, and the third is the one that decided it:
//!
//!   * `spec::secret` already works this way. It returns a `BTreeMap` of environment variables,
//!     not a `Secret`, and nothing about it is worse for that.
//!   * The fake would otherwise have to reach into a `Deployment`'s labels to answer "which room
//!     is this?", making every lifecycle test depend on the label rendering being right — two
//!     properties tangled into one failure.
//!   * The manifest is where the cluster's own vocabulary belongs (`ownerReferences`,
//!     `ipFamilyPolicy`, `sharing-key`), and none of it is a decision the reconciler makes.
//!
//! The manifest builders and the real client land at M7; the trait and the fake land here so the
//! lifecycle is testable before either exists.
#[cfg(test)]
pub mod fake;
pub mod kube;

use async_trait::async_trait;
use puna_core::ids::RoomId;

use crate::spec::secret::SecretData;

/// Every room object is named `mw-<room-id>`.
///
/// 39 characters with a UUID, comfortably inside the 63-character RFC 1035 limit Service names are
/// held to — and the bare id fits a label *value* untruncated too. So nothing here ever shortens or
/// hashes an id, which is worth stating because a truncation introduced later would collide
/// silently rather than fail.
pub fn object_name(room: RoomId) -> String {
    format!("mw-{room}")
}

/// What the cluster can go wrong with, classified by what the caller should do about it.
///
/// The classification is the point. §7's rule is that **the tick is the retry** — nothing loops
/// inside one pass — so an error only has to answer "try again next tick, or stop touching this
/// room?".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClusterError {
    /// `409`. **Success, not a failure**: the object is there, which is what was wanted. The
    /// caller re-`get`s for the uid rather than treating it as an error.
    #[error("{name} already exists")]
    AlreadyExists { name: String },

    // There is deliberately no `NotFound`. A `get` returns `Option`, and a delete of something
    // already gone is success -- teardown runs every tick until the row is gone, so anything else
    // would turn a completed delete into an error that never clears.
    //
    /// `403`, or a `404` on the API group itself: RBAC or a wrong apiVersion. Retrying cannot fix
    /// it, so the room stops rather than failing the same way every 30 seconds forever.
    #[error("fatal cluster error: {0}")]
    Fatal(String),

    /// `429`, `5xx`, or transport. The next tick sees the same world and tries again.
    #[error("transient cluster error: {0}")]
    Transient(String),
}

impl ClusterError {
    /// Whether retrying is pointless.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }
}

pub type Result<T> = std::result::Result<T, ClusterError>;

/// A Deployment's owner, as a Service or Secret refers to it.
///
/// **The uid is not optional and this type is why.** An `ownerReference` without one is silently
/// ignored by the garbage collector: the object simply never gets collected, and the leak shows up
/// weeks later as Services holding ports for rooms that no longer exist. Making the uid a required
/// field of a required struct means the only way to write an ownerReference is to have the uid in
/// hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRef {
    pub name: String,
    pub uid: String,
}

/// A room's pod, as Puna describes it. Rendered into a Deployment by the implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSpec {
    pub room_id: RoomId,
    /// Covers the spec proper **and `slot_auth`** — the password mode reaches pahoa through the
    /// Secret via `envFrom`, so it moves nothing in the Deployment and would otherwise never
    /// trigger a recreate. Password *values* stay out, which is what keeps per-slot rotation live.
    pub spec_hash: String,
    pub image: String,
    /// The even half of the reserved pair; the filtered listener is `base_port + 1` by
    /// construction, never allocated separately.
    pub base_port: u16,
    /// Whether to publish the filtered feed. On by default: the pair is reserved either way and
    /// the filtered listener is the same server, so turning it off is the unusual choice.
    pub wants_filtered: bool,
    /// Every slot in the multidata, groups included — pahoa sizes its outbound budget from
    /// `slot_info.len()`, so the connectable count would under-request memory.
    pub slot_count: i32,
    pub save_interval_secs: i32,
    pub use_embedded_options: bool,
}

impl RoomSpec {
    pub fn name(&self) -> String {
        object_name(self.room_id)
    }
}

/// A room's LoadBalancer Service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    pub room_id: RoomId,
    pub base_port: u16,
    pub wants_filtered: bool,
    /// Always present: a Service created before its Deployment's uid is known would outlive the
    /// room. §7 orders it after the Deployment for exactly this reason.
    pub owner: OwnerRef,
}

impl ServiceSpec {
    pub fn name(&self) -> String {
        object_name(self.room_id)
    }
}

/// A room's Secret: the environment `spec::secret::build` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSpec {
    pub room_id: RoomId,
    pub data: SecretData,
    /// `None` on the first apply, because the Deployment it would point at does not exist yet.
    /// §7 applies it twice on purpose: once unowned so the pod can start, once owned so the
    /// garbage collector takes it away with the room.
    pub owner: Option<OwnerRef>,
}

impl SecretSpec {
    pub fn name(&self) -> String {
        object_name(self.room_id)
    }
}

/// A Deployment as the reconciler sees it. Seven fields, deliberately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomDeployment {
    pub name: String,
    pub uid: String,
    /// `None` when the room label is missing or unparseable, which makes it an orphan by
    /// definition — there is no room row it could belong to.
    pub room_id: Option<RoomId>,
    pub spec_hash: Option<String>,
    pub replicas: i32,
    pub ready_replicas: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A Service as the reconciler sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomService {
    pub name: String,
    pub room_id: Option<RoomId>,
    /// `status.loadBalancer.ingress[0].ip`, once IPAM has answered. `None` means "not yet", and
    /// **a value that is not the configured address is the silent Cilium failure** — the room is
    /// live on an address DNS never mentions, so §5 quarantines the pair rather than serving it.
    pub ingress_ip: Option<String>,
    /// `None` means nothing will ever garbage-collect this: either the ownerReference was written
    /// without a uid or the Deployment it named is already gone.
    pub owner_uid: Option<String>,
}

/// A Secret as the reconciler sees it. Never its contents — a sweep needs to know a Secret exists
/// and whether it is owned, and reading the values back would be the one call that puts every
/// room's credentials through the orchestrator's logs on a bad day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSecret {
    pub name: String,
    pub room_id: Option<RoomId>,
    pub owner_uid: Option<String>,
}

/// The whole managed world, read once per tick.
///
/// Three list calls regardless of room count, rather than a `get` per room: at several hundred
/// rooms the difference is a handful of requests against a thousand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClusterSnapshot {
    pub deployments: Vec<RoomDeployment>,
    pub services: Vec<RoomService>,
    pub secrets: Vec<RoomSecret>,
}

impl ClusterSnapshot {
    pub fn deployment(&self, room: RoomId) -> Option<&RoomDeployment> {
        self.deployments.iter().find(|d| d.room_id == Some(room))
    }

    /// Neither of these has a production caller yet: nothing in the room lifecycle asks about a
    /// Service or a Secret by room — the Deployment is the object the state machine turns on, and the
    /// other two follow it through ownership. **M9's sweep is what reads them**, to find the ones
    /// whose owner is gone. The lifecycle tests use them today to assert that ownership landed.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "M9's orphan sweep reads these; the tests already do"
        )
    )]
    pub fn service(&self, room: RoomId) -> Option<&RoomService> {
        self.services.iter().find(|s| s.room_id == Some(room))
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "M9's orphan sweep reads these; the tests already do"
        )
    )]
    pub fn secret(&self, room: RoomId) -> Option<&RoomSecret> {
        self.secrets.iter().find(|s| s.room_id == Some(room))
    }
}

/// Everything the orchestrator does to a cluster.
#[async_trait]
pub trait ClusterApi: Send + Sync {
    /// Label-selected on `app.kubernetes.io/managed-by=puna`. Objects Puna did not create are
    /// invisible here, which is what makes "orphan" mean "ours, with no room" rather than
    /// "somebody else's".
    async fn list_deployments(&self) -> Result<Vec<RoomDeployment>>;
    async fn list_services(&self) -> Result<Vec<RoomService>>;
    async fn list_secrets(&self) -> Result<Vec<RoomSecret>>;

    async fn get_deployment(&self, name: &str) -> Result<Option<RoomDeployment>>;
    /// Returns `.metadata.uid`, which the caller must persist **before doing anything else** —
    /// losing it means nothing can ever be owned by this Deployment, and unowned objects are not
    /// collected.
    async fn create_deployment(&self, spec: &RoomSpec) -> Result<String>;
    async fn delete_deployment(&self, name: &str) -> Result<()>;

    /// Server-side apply, so writing it repeatedly is free and it is always current.
    async fn apply_secret(&self, spec: &SecretSpec) -> Result<()>;
    async fn delete_secret(&self, name: &str) -> Result<()>;

    async fn create_service(&self, spec: &ServiceSpec) -> Result<()>;
    async fn get_service(&self, name: &str) -> Result<Option<RoomService>>;

    /// The whole world in one call.
    ///
    /// A provided method rather than a required one: an implementation has nothing to add, and the
    /// three lists it composes are the ones the sweep needs individually anyway.
    async fn snapshot(&self) -> Result<ClusterSnapshot> {
        Ok(ClusterSnapshot {
            deployments: self.list_deployments().await?,
            services: self.list_services().await?,
            secrets: self.list_secrets().await?,
        })
    }
}
