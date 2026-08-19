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
pub mod room;
pub mod secret;

/// The room's own state directory: `rooms/<id>` on the shared volume, by `subPath`.
///
/// Holds one Puna-written file and two of pahoa's. **Puna never writes `room.lock` or `room.save`.**
pub const SAVE_DIR: &str = "/var/lib/pahoa";

/// The seed, copied in at provisioning so a room is self-contained — generation retention can never
/// make an existing room unstartable.
pub const SEED_PATH: &str = "/var/lib/pahoa/seed.archipelago";

/// `shared/` on the volume, mounted read-only into every room.
///
/// The argv names the file, the manifest names the directory — so this half has no reader until the
/// Deployment builder lands at M7.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "a volumeMount path; the Deployment builder lands at M7"
    )
)]
pub const SHARED_DIR: &str = "/shared";

/// The data package snapshot. Without it, games resolve from the seed's embedded package alone,
/// which covers names and ids but never hint blacklists.
pub const SNAPSHOT_PATH: &str = "/shared/datapackage.json";

/// Where the room certificate is mounted. One Certificate for the single name every room shares,
/// which differs only by port — and pahoa reloads it in place, so a renewal needs no restart.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "a volumeMount path; the Deployment builder lands at M7"
    )
)]
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
        assert!(SNAPSHOT_PATH.starts_with(&format!("{SHARED_DIR}/")));
        assert!(TLS_CERT_PATH.starts_with(&format!("{TLS_DIR}/")));
        assert!(TLS_KEY_PATH.starts_with(&format!("{TLS_DIR}/")));
        // Three distinct mounts, so none of them can be satisfied by another's volume.
        assert!(!SHARED_DIR.starts_with(SAVE_DIR));
        assert!(!TLS_DIR.starts_with(SAVE_DIR));
    }

    /// cert-manager writes exactly these two keys into a TLS Secret; renaming either here would
    /// mount a file pahoa cannot find, and pahoa's error names the path rather than the cause.
    #[test]
    fn the_certificate_keys_are_the_ones_cert_manager_writes() {
        assert!(TLS_CERT_PATH.ends_with("/tls.crt"));
        assert!(TLS_KEY_PATH.ends_with("/tls.key"));
    }
}
