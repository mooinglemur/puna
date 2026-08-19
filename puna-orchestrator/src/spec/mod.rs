//! Turning a room row into the description of the objects that serve it.
//!
//! Pure functions over `puna-core` types and [`crate::cluster`]'s intent structs: nothing here talks
//! to a cluster, which is what lets the whole lifecycle be tested against `FakeCluster`.
//!
//! ## The paths below are shared on purpose
//!
//! `--save-dir` and the `volumeMount` that makes it exist are one fact stated twice, and a
//! disagreement between them is not a startup error — it is a room that comes up, serves players,
//! and persists nothing. Same for the certificate: pahoa reads `--tls-cert` from a path only the
//! Secret volume provides. So the argv and the manifest read the same constants rather than
//! matching string literals.

pub mod args;
pub mod deployment;
pub mod room;
pub mod secret;
pub mod service;

use std::collections::BTreeMap;

use puna_core::ids::RoomId;

/// The cluster-wide values a room's manifest needs and no room chooses.
///
/// One namespace, one public address, one certificate, one volume — a room differs from its
/// neighbours only by id and port. Kept apart from [`crate::cluster::RoomSpec`] for that reason:
/// these are **not** in the spec hash, because a change to any of them is an operator editing the
/// orchestrator's own Deployment, and hashing them would recreate every room in the namespace at
/// once as a side effect of a config edit.
#[derive(Debug, Clone)]
pub struct Site {
    pub namespace: String,
    /// The shared public address every room Service must land on, read back and asserted after
    /// creation because a mismatch is the silent Cilium failure.
    pub lb_ip: String,
    pub lb_sharing_key: String,
    /// The room certificate's Secret. One name for every room, since they share a hostname.
    pub tls_secret: String,
    /// The CephFS PVC holding `generations/`, `rooms/`, `shared/` and `trash/`.
    pub data_pvc: String,
}

// A `from_config` constructor belongs here and is deliberately absent until there is a caller: the
// tick builds the `Site` once, when it is rewired. An untested mapping of five same-typed String
// fields is exactly the shape that silently swaps two of them.

/// `app.kubernetes.io/managed-by=puna` — what makes an object Puna's to reason about.
///
/// Every list is selected on it, so "orphan" can mean "ours, with no room" rather than "somebody
/// else's". An object created without it is invisible to the sweep and will never be collected.
pub const MANAGED_BY_KEY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY: &str = "puna";
pub const NAME_KEY: &str = "app.kubernetes.io/name";
pub const NAME: &str = "pahoa";

/// The room id, as a label value.
///
/// A UUID is 36 characters and a label value allows 63, so the id goes in whole. Worth stating
/// because truncating or hashing it later would collide silently rather than fail.
pub const ROOM_KEY: &str = "onelemur.com/room";

/// Which LoadBalancer IP pool a Service draws from.
///
/// **Required on room Services**, not decorative: the `br-v5` pool's `NotIn` selector matches
/// anything without it, so an unlabeled Service lands on `172.29.0.x` — a private address, from
/// which the room is simply unreachable.
pub const LB_POOL_KEY: &str = "onelemur.com/lb-pool";
pub const LB_POOL: &str = "public";

/// Where the spec fingerprint rides. Read back off a live Deployment and compared with the row.
pub const SPEC_HASH_ANNOTATION: &str = "puna.onelemur.com/spec-hash";

/// The room pods' ServiceAccount, which exists to have **no token mounted**. That is the mechanical
/// half of the tier split: a room cannot reach the Kubernetes API even in principle.
pub const ROOM_SERVICE_ACCOUNT: &str = "puna-room";

/// The label selector every list call uses.
pub fn managed_selector() -> String {
    format!("{MANAGED_BY_KEY}={MANAGED_BY}")
}

/// The labels that identify one room's pods, and nothing more.
///
/// **This is the Deployment's `selector.matchLabels` and the Service's `selector`, from one
/// function.** Two hand-written copies that disagree produce a Service with no endpoints, which
/// presents as a connection refused on a room that Kubernetes reports as perfectly healthy.
pub fn selector_labels(room: RoomId) -> BTreeMap<String, String> {
    BTreeMap::from([(ROOM_KEY.to_string(), room.to_string())])
}

/// The full label set for an object belonging to one room.
pub fn labels(room: RoomId) -> BTreeMap<String, String> {
    let mut labels = selector_labels(room);
    labels.insert(MANAGED_BY_KEY.to_string(), MANAGED_BY.to_string());
    labels.insert(NAME_KEY.to_string(), NAME.to_string());
    labels
}

/// The room id carried by an object's labels, if it has one that parses.
///
/// `None` makes it an orphan by definition — there is no room row it could belong to.
pub fn room_of(labels: &BTreeMap<String, String>) -> Option<RoomId> {
    labels
        .get(ROOM_KEY)?
        .parse::<uuid::Uuid>()
        .ok()
        .map(RoomId::from)
}

/// The room's own state directory: `rooms/<id>` on the shared volume, by `subPath`.
///
/// Holds one Puna-written file and three of pahoa's. **Puna never writes `room.lock`, `room.save`
/// or `history.jsonl`.**
pub const SAVE_DIR: &str = "/var/lib/pahoa";

// The journal is `history.jsonl` inside SAVE_DIR, and Puna never names that path: pahoa derives it
// from `--save-dir` itself. The constant lands with its reader -- the organizer download, which
// reaches it as `rooms/<id>/history.jsonl` on the volume rather than at the container's path.

/// The seed, copied in at provisioning so a room is self-contained — generation retention can never
/// make an existing room unstartable.
pub const SEED_PATH: &str = "/var/lib/pahoa/seed.archipelago";

// There is deliberately no `/shared` mount and no data package snapshot path.
//
// Rooms briefly took `--snapshot=/shared/datapackage.json`, which carried `hint_blacklist` -- the
// one thing the reference server reads from installed apworlds and that is never serialized into a
// multidata. Pahoa now compiles that table into the binary (`pahoa-multidata/src/hint_blacklist.rs`)
// and has REMOVED the option, so there is nothing to mount and sending the flag is a hard `exit 1`.
//
// Everything else a room needs -- names, ids, name groups, checksums -- was always in the seed.

/// Where the room certificate is mounted. One Certificate for the single name every room shares,
/// which differs only by port — and pahoa reloads it in place, so a renewal needs no restart.
pub const TLS_DIR: &str = "/etc/pahoa/tls";
pub const TLS_CERT_PATH: &str = "/etc/pahoa/tls/tls.crt";
pub const TLS_KEY_PATH: &str = "/etc/pahoa/tls/tls.key";

#[cfg(test)]
mod tests {
    use super::*;

    /// A path pahoa is told to read must be inside something the pod mounts. Cheap to assert, and
    /// the failure it prevents is silent: pahoa would start, find nothing, and serve an empty room.
    #[test]
    fn every_path_is_under_its_mount() {
        assert!(SEED_PATH.starts_with(&format!("{SAVE_DIR}/")));
        assert!(TLS_CERT_PATH.starts_with(&format!("{TLS_DIR}/")));
        assert!(TLS_KEY_PATH.starts_with(&format!("{TLS_DIR}/")));
        // Two distinct mounts, so neither can be satisfied by the other's volume.
        assert!(!TLS_DIR.starts_with(SAVE_DIR));
    }

    /// cert-manager writes exactly these two keys into a TLS Secret; renaming either here would
    /// mount a file pahoa cannot find, and pahoa's error names the path rather than the cause.
    #[test]
    fn the_certificate_keys_are_the_ones_cert_manager_writes() {
        assert!(TLS_CERT_PATH.ends_with("/tls.crt"));
        assert!(TLS_KEY_PATH.ends_with("/tls.key"));
    }

    #[test]
    fn a_rooms_labels_carry_its_id_and_survive_a_round_trip() {
        let room = RoomId::new();
        let labels = labels(room);

        assert_eq!(room_of(&labels), Some(room));
        assert_eq!(
            labels.get(MANAGED_BY_KEY).map(String::as_str),
            Some(MANAGED_BY)
        );
        // The id goes in whole: 36 characters against a label value's 63.
        assert!(labels[ROOM_KEY].len() <= 63);

        // Anything else is an orphan rather than a room, including a label that is present and not
        // a uuid -- guessing would attach a live pod to the wrong row.
        assert_eq!(room_of(&BTreeMap::new()), None);
        assert_eq!(
            room_of(&BTreeMap::from([(
                ROOM_KEY.to_string(),
                "not-a-uuid".to_string()
            )])),
            None
        );
    }

    /// The selector is a subset of the labels, or a Service selects nothing.
    #[test]
    fn the_selector_is_a_subset_of_the_labels() {
        let room = RoomId::new();
        let labels = labels(room);
        for (key, value) in selector_labels(room) {
            assert_eq!(labels.get(&key), Some(&value));
        }
        // Only the room id: adding a label to the selector would orphan every pod created before
        // the change, since selectors are immutable on a Deployment.
        assert_eq!(selector_labels(room).len(), 1);
    }
}
