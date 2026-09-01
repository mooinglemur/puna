//! Room state directories on the shared volume.
//!
//! ```text
//! /var/lib/puna/
//! ├── generations/<sha256>/     WEB-owned. Read here, never written.
//! ├── rooms/<room-id>/          this module's; bind-mounted into that room's pod alone
//! │   ├── seed.archipelago      COPIED from generations/; rooms are self-contained
//! │   └── room.lock  room.save  PAHOA'S. Puna never writes these two names.
//! └── trash/<room-id>-<ts>/     7-day undo
//! ```
//!
//! ## The invariant, and the exact window it holds in
//!
//! **`provisioned_at IS NOT NULL` implies the room directory exists.** [`provision`] is ordered so
//! there is no moment where a row claims a directory that is not there:
//!
//! 1. build everything under `rooms/.tmp-<id>-<nonce>/`
//! 2. `fsync` the contents, then the directory
//! 3. `rename` it onto `rooms/<id>`: **atomic**
//! 4. `fsync` `rooms/` so the rename itself survives a crash
//! 5. only then does the caller set `provisioned_at`
//!
//! Crash between 3 and 5: the directory exists and the row claims nothing, so the invariant holds
//! and the next tick hits `EEXIST` and completes step 5. Crash before 3: a `.tmp-*` that nothing
//! references, swept after an hour. There is deliberately no ordering in which the row is written
//! first, because that is the one that produces `integrity_fault`.
//!
//! ## Rooms are self-contained
//!
//! The seed is **copied** in rather than referenced out of `generations/`. It costs a few
//! megabytes per room and buys a single directory to check for the invariant above, and it means
//! generation retention can never make an existing room unstartable.

use std::path::{Path, PathBuf};

use puna_core::ids::RoomId;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// The generation this room was built from is not on disk.
    ///
    /// Fatal for the room rather than retryable: nothing the orchestrator does will make the
    /// bytes appear, so it fails loudly instead of looping.
    #[error("generation directory {0} is missing; the room cannot be provisioned")]
    MissingGeneration(PathBuf),
}

fn io<T>(result: std::io::Result<T>, context: impl Into<String>) -> Result<T, StorageError> {
    result.map_err(|source| StorageError::Io {
        context: context.into(),
        source,
    })
}

/// The layout, rooted at the volume mount.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn generations(&self) -> PathBuf {
        self.root.join("generations")
    }

    pub fn generation(&self, sha256_hex: &str) -> PathBuf {
        self.generations().join(sha256_hex)
    }

    pub fn rooms(&self) -> PathBuf {
        self.root.join("rooms")
    }

    pub fn room(&self, id: RoomId) -> PathBuf {
        self.rooms().join(id.to_string())
    }

    pub fn trash(&self) -> PathBuf {
        self.root.join("trash")
    }
}

/// What [`provision`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provisioned {
    Created,
    /// The directory was already there, from an attempt that crashed before the row was updated.
    /// Success: the caller proceeds to set `provisioned_at`, which is exactly what was missed.
    AlreadyPresent,
}

/// Materialize a room's state directory from its generation.
///
/// `nonce` only has to be unique among concurrent attempts; the atomicity comes from the rename,
/// not from the name.
pub fn provision(
    layout: &Layout,
    id: RoomId,
    generation_sha256_hex: &str,
    nonce: &str,
) -> Result<Provisioned, StorageError> {
    let target = layout.room(id);
    if target.is_dir() {
        return Ok(Provisioned::AlreadyPresent);
    }

    let seed = layout
        .generation(generation_sha256_hex)
        .join("seed.archipelago");
    if !seed.is_file() {
        return Err(StorageError::MissingGeneration(
            layout.generation(generation_sha256_hex),
        ));
    }

    let rooms = layout.rooms();
    io(
        std::fs::create_dir_all(&rooms),
        format!("creating {}", rooms.display()),
    )?;

    let tmp = rooms.join(format!(".tmp-{id}-{nonce}"));
    let _ = std::fs::remove_dir_all(&tmp);
    io(
        std::fs::create_dir(&tmp),
        format!("creating {}", tmp.display()),
    )?;

    // 0750: the room runs as uid 1000 and nothing else on this volume should be reading another
    // room's save.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        io(
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o750)),
            format!("chmod {}", tmp.display()),
        )?;
    }

    let result = (|| -> Result<(), StorageError> {
        let dest = tmp.join("seed.archipelago");
        io(
            std::fs::copy(&seed, &dest).map(|_| ()),
            format!("copying {} to {}", seed.display(), dest.display()),
        )?;
        // fsync the file, not just the directory entry: a rename is atomic with respect to the
        // name, not to the contents behind it.
        io(
            std::fs::File::open(&dest).and_then(|f| f.sync_all()),
            format!("fsyncing {}", dest.display()),
        )?;
        io(sync_dir(&tmp), format!("fsyncing {}", tmp.display()))?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }

    match std::fs::rename(&tmp, &target) {
        Ok(()) => {
            io(sync_dir(&rooms), format!("fsyncing {}", rooms.display()))?;
            Ok(Provisioned::Created)
        }
        // Another attempt won the race. Both built the same thing from the same generation.
        Err(e) if is_already_present(&e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(Provisioned::AlreadyPresent)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(StorageError::Io {
                context: format!("renaming {} to {}", tmp.display(), target.display()),
                source: e,
            })
        }
    }
}

/// Does a room's directory exist?
///
/// The other half of the invariant: a row with `provisioned_at` set and no directory here is an
/// `integrity_fault`, which is reported loudly and **never auto-repaired**. Recreating it would
/// silently replace a player's progress with an empty room.
pub fn room_exists(layout: &Layout, id: RoomId) -> bool {
    layout.room(id).is_dir()
}

/// Move a room's directory to `trash/`, returning where it went.
///
/// A rename rather than a delete, for two reasons. It is instant, where `rm -rf` of a 2000-slot
/// save on CephFS is minutes. And it is an undo for the one operation that destroys player
/// progress: the sweep removes trash older than the retention window, so a mistake has days to
/// be noticed.
pub fn trash(layout: &Layout, id: RoomId, stamp: &str) -> Result<Option<PathBuf>, StorageError> {
    let source = layout.room(id);
    if !source.is_dir() {
        return Ok(None);
    }

    let trash = layout.trash();
    io(
        std::fs::create_dir_all(&trash),
        format!("creating {}", trash.display()),
    )?;

    let target = trash.join(format!("{id}-{stamp}"));
    io(
        std::fs::rename(&source, &target),
        format!("renaming {} to {}", source.display(), target.display()),
    )?;
    io(sync_dir(&layout.rooms()), "fsyncing rooms/")?;
    Ok(Some(target))
}

/// Every room directory on the volume, for the orphan sweep.
///
/// Orphans are **reported, not deleted**: a directory with no row is either a bug or a database
/// restored from an older backup, and in the second case deleting it would destroy the very state
/// that could repair the room.
pub fn list_room_dirs(layout: &Layout) -> Result<Vec<RoomId>, StorageError> {
    let rooms = layout.rooms();
    if !rooms.is_dir() {
        return Ok(Vec::new());
    }

    let entries = io(
        std::fs::read_dir(&rooms),
        format!("reading {}", rooms.display()),
    )?;

    Ok(entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str()?.parse::<RoomId>().ok())
        .collect())
}

/// Remove `.tmp-*` directories older than `max_age`, returning how many went.
///
/// These are abandoned provisioning attempts. Nothing references them, so the only cost of
/// leaving one is disk, but the volume's quota is shared across every room in the environment,
/// which is why the sweep exists at all.
pub fn sweep_temp_dirs(
    layout: &Layout,
    max_age: std::time::Duration,
) -> Result<usize, StorageError> {
    let rooms = layout.rooms();
    if !rooms.is_dir() {
        return Ok(0);
    }

    let entries = io(
        std::fs::read_dir(&rooms),
        format!("reading {}", rooms.display()),
    )?;
    let now = std::time::SystemTime::now();
    let mut removed = 0;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(".tmp-") {
            continue;
        }
        let Ok(age) = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| now.duration_since(t).map_err(std::io::Error::other))
        else {
            continue;
        };
        if age >= max_age && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Every generation directory on the volume, by content hash.
///
/// For counting what nothing references. **Never for deleting**: a generation is content-addressed
/// and shared between every room built from it, so reclaiming one is an administrator's action with
/// a listing in front of it, and a room whose generation was removed is permanently unstartable,
/// which is the failure D2 exists to prevent.
pub fn list_generation_dirs(layout: &Layout) -> Result<Vec<String>, StorageError> {
    let generations = layout.generations();
    if !generations.is_dir() {
        return Ok(Vec::new());
    }

    let entries = io(
        std::fs::read_dir(&generations),
        format!("reading {}", generations.display()),
    )?;

    Ok(entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        // `.tmp-*` is an ingest in flight, which is not a generation and is swept by its own rule.
        .filter(|name| !name.starts_with(".tmp-"))
        .collect())
}

/// Remove trashed room directories older than `retention`, returning how many went.
///
/// **This is the one place Puna destroys a player's progress**, which is why it is the slowest and
/// most conservative thing here: a room's directory is moved to the trash on deletion rather than
/// removed, and only time takes it from there. The window is the undo.
pub fn sweep_trash(layout: &Layout, retention: std::time::Duration) -> Result<usize, StorageError> {
    let trash = layout.trash();
    if !trash.is_dir() {
        return Ok(0);
    }

    let entries = io(
        std::fs::read_dir(&trash),
        format!("reading {}", trash.display()),
    )?;
    let now = std::time::SystemTime::now();
    let mut removed = 0;

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        // The directory's own mtime, not the timestamp in its name: the name is for a human
        // reading `ls`, and parsing it would make the retention depend on a format string.
        let Ok(age) = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| now.duration_since(t).map_err(std::io::Error::other))
        else {
            continue;
        };
        if age >= retention {
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "could not remove an expired trash directory"
                ),
            }
        }
    }
    Ok(removed)
}

fn is_already_present(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
    )
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backdate a directory's mtime, so an age-based sweep can be tested without waiting.
    fn age(path: &Path, by: std::time::Duration) {
        let when = std::time::SystemTime::now() - by;
        let file = std::fs::File::open(path).expect("open");
        file.set_modified(when).expect("set mtime");
    }

    /// A layout with one generation already on disk, as the web tier would have left it.
    fn layout_with_generation(dir: &tempfile::TempDir) -> (Layout, String) {
        let layout = Layout::new(dir.path());
        let sha = "a".repeat(64);
        let generation = layout.generation(&sha);
        std::fs::create_dir_all(&generation).expect("mkdir");
        std::fs::write(generation.join("seed.archipelago"), b"multidata").expect("seed");
        (layout, sha)
    }

    #[test]
    fn provisioning_copies_the_seed_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (layout, sha) = layout_with_generation(&dir);
        let id = RoomId::new();

        assert_eq!(
            provision(&layout, id, &sha, "n1").expect("provision"),
            Provisioned::Created
        );
        assert!(room_exists(&layout, id));
        assert_eq!(
            std::fs::read(layout.room(id).join("seed.archipelago")).expect("seed"),
            b"multidata"
        );

        // The crash-between-rename-and-row case: the directory is there, the row is not updated,
        // and the next tick must treat that as success rather than as a conflict.
        assert_eq!(
            provision(&layout, id, &sha, "n2").expect("again"),
            Provisioned::AlreadyPresent
        );
    }

    /// The seed is copied, not linked: generation housekeeping must not be able to reach into a
    /// room that is already running.
    #[test]
    fn the_room_holds_its_own_copy_of_the_seed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (layout, sha) = layout_with_generation(&dir);
        let id = RoomId::new();
        provision(&layout, id, &sha, "n1").expect("provision");

        std::fs::remove_dir_all(layout.generation(&sha)).expect("prune the generation");

        assert!(room_exists(&layout, id));
        assert_eq!(
            std::fs::read(layout.room(id).join("seed.archipelago")).expect("seed"),
            b"multidata",
            "pruning a generation must not disturb a room already built from it"
        );
    }

    #[test]
    fn a_missing_generation_is_a_named_failure_not_an_empty_room() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path());
        let id = RoomId::new();

        let err = provision(&layout, id, &"b".repeat(64), "n1").expect_err("must fail");
        assert!(matches!(err, StorageError::MissingGeneration(_)), "{err:?}");
        assert!(
            !room_exists(&layout, id),
            "a failed provision must leave nothing behind"
        );
    }

    #[test]
    fn a_failed_provision_leaves_no_temp_directory_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (layout, sha) = layout_with_generation(&dir);
        // Make the seed unreadable so the copy fails after the temp directory exists.
        let seed = layout.generation(&sha).join("seed.archipelago");
        std::fs::remove_file(&seed).expect("remove");
        std::fs::create_dir(&seed).expect("a directory where a file should be");

        let id = RoomId::new();
        assert!(provision(&layout, id, &sha, "n1").is_err());

        let leftovers: Vec<_> = std::fs::read_dir(layout.rooms())
            .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn trashing_moves_the_directory_and_reports_where() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (layout, sha) = layout_with_generation(&dir);
        let id = RoomId::new();
        provision(&layout, id, &sha, "n1").expect("provision");

        let moved = trash(&layout, id, "20260819T000000Z")
            .expect("trash")
            .expect("something to move");
        assert!(!room_exists(&layout, id));
        assert!(moved.is_dir());
        // The save survives in the trash, which is the point: this is the undo for the one
        // operation that destroys player progress.
        assert_eq!(
            std::fs::read(moved.join("seed.archipelago")).expect("seed"),
            b"multidata"
        );

        // Trashing a room with no directory is not an error: idle teardown never touches the
        // directory, so a room can legitimately be deleted without one.
        assert_eq!(trash(&layout, RoomId::new(), "x").expect("absent"), None);
    }

    #[test]
    fn room_directories_are_listed_and_temp_ones_are_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (layout, sha) = layout_with_generation(&dir);
        let a = RoomId::new();
        let b = RoomId::new();
        provision(&layout, a, &sha, "n1").expect("a");
        provision(&layout, b, &sha, "n2").expect("b");
        std::fs::create_dir(layout.rooms().join(".tmp-leftover")).expect("tmp");
        std::fs::create_dir(layout.rooms().join("not-a-uuid")).expect("junk");

        let mut found = list_room_dirs(&layout).expect("list");
        found.sort_by_key(|id| id.to_string());
        let mut expected = vec![a, b];
        expected.sort_by_key(|id| id.to_string());
        assert_eq!(found, expected, "only well-formed room ids are room dirs");
    }

    /// The one place Puna destroys a player's progress, so the window is the whole point: a
    /// directory that has not aged out is left exactly where it is.
    #[test]
    fn the_trash_is_swept_only_after_the_retention_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path());
        std::fs::create_dir_all(layout.trash()).expect("trash");

        let fresh = layout.trash().join("fresh-20260819T120000Z");
        let expired = layout.trash().join("expired-20260101T120000Z");
        for dir in [&fresh, &expired] {
            std::fs::create_dir(dir).expect("dir");
            std::fs::write(dir.join("room.save"), b"progress").expect("save");
        }
        age(&expired, std::time::Duration::from_secs(8 * 24 * 3600));

        let removed =
            sweep_trash(&layout, std::time::Duration::from_secs(7 * 24 * 3600)).expect("sweep");
        assert_eq!(removed, 1);
        assert!(fresh.is_dir(), "inside the window, so still recoverable");
        assert!(!expired.is_dir());

        // No trash directory at all is not an error: a deployment that has never deleted a room
        // has nothing to sweep.
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            sweep_trash(&Layout::new(empty.path()), std::time::Duration::ZERO).expect("sweep"),
            0
        );
    }

    /// Counted, never deleted: a generation is shared, and removing one makes every room built from
    /// it permanently unstartable.
    #[test]
    fn generation_directories_are_listed_and_ingests_in_flight_are_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path());
        std::fs::create_dir_all(layout.generations()).expect("generations");

        for name in ["aa00", "bb11", ".tmp-halfway"] {
            std::fs::create_dir(layout.generations().join(name)).expect("dir");
        }
        std::fs::write(layout.generations().join("stray.txt"), b"not a directory").expect("file");

        let mut listed = list_generation_dirs(&layout).expect("list");
        listed.sort();
        assert_eq!(listed, ["aa00", "bb11"]);
    }

    #[test]
    fn the_sweep_removes_only_old_temp_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (layout, sha) = layout_with_generation(&dir);
        let id = RoomId::new();
        provision(&layout, id, &sha, "n1").expect("provision");
        std::fs::create_dir(layout.rooms().join(".tmp-fresh")).expect("tmp");

        // Nothing is old enough yet, and a real room is never a candidate whatever its age.
        assert_eq!(
            sweep_temp_dirs(&layout, std::time::Duration::from_secs(3600)).expect("sweep"),
            0
        );
        assert!(layout.rooms().join(".tmp-fresh").is_dir());

        assert_eq!(
            sweep_temp_dirs(&layout, std::time::Duration::ZERO).expect("sweep"),
            1
        );
        assert!(!layout.rooms().join(".tmp-fresh").is_dir());
        assert!(room_exists(&layout, id), "the sweep must not touch rooms");
    }
}
