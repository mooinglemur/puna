//! Content-addressed promotion, against a real filesystem and a real database.
//!
//! Gated on `PUNA_TEST_GENERATION_ZIP` for the parts that need a real archive, because the
//! interesting properties -- what gets extracted, and that re-uploading converges -- are not
//! observable against a synthetic zip with no multidata in it.
//!
//! The property under test is that **dedup and idempotence are the same mechanism** on both
//! sides: the directory is named after the content hash, and `generations.sha256` is unique, so
//! uploading the same bytes twice converges on one directory and one row without either side
//! needing a check-then-write.

mod common;

use std::path::Path;

use common::with_db;
use puna_core::artifact::{self, Promotion};
use puna_core::model::{generation, user};

const LIMIT: u64 = 512 * 1024 * 1024;
const UPLOADER: i64 = 4242;

fn fixture() -> Option<Vec<u8>> {
    let path = std::env::var("PUNA_TEST_GENERATION_ZIP").ok()?;
    let path = match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").ok()?),
        None => path,
    };
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) => panic!("PUNA_TEST_GENERATION_ZIP={path} could not be read: {e}"),
    }
}

/// Everything `inspect` promised is on disk afterwards, under the hash as its name.
#[test]
fn a_generation_lands_on_disk_under_its_hash() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };
    let meta = artifact::inspect(&bytes, LIMIT).expect("inspect");
    let dir = tempfile::tempdir().expect("tempdir");

    let (paths, outcome) =
        artifact::promote(dir.path(), &bytes, &meta, "nonce-1").expect("promote");
    assert_eq!(outcome, Promotion::Stored);

    assert!(paths.root.is_dir(), "{}", paths.root.display());
    assert_eq!(
        paths.root.file_name().and_then(|n| n.to_str()),
        Some(artifact::storage::hex(&meta.sha256).as_str()),
        "the directory name is the content hash"
    );

    // The original is kept: puna re-serves patches with the server address embedded, and a
    // generation whose bytes were discarded could not be re-extracted after a code change.
    assert_eq!(
        std::fs::read(paths.archive()).expect("archive").len(),
        bytes.len()
    );
    assert!(std::fs::metadata(paths.seed()).expect("seed").len() > 0);
    assert_eq!(paths.spoiler().exists(), meta.spoiler_member.is_some());

    for slot in &meta.slots {
        let Some(member) = &slot.patch_member else {
            continue;
        };
        // The extension survives, the player's name does not: the file is named by slot number so
        // that no user-controlled text reaches the filesystem.
        let extension = member.rsplit_once('.').map(|(_, e)| e).unwrap_or("bin");
        let expected = paths.patch(slot.slot_number, &extension.to_ascii_lowercase());
        assert!(
            expected.exists(),
            "slot {} patch missing at {}",
            slot.slot_number,
            expected.display()
        );
        assert!(!contains_name(&paths.root, &slot.player_name));
    }
}

/// Is any file under `root` named after this player?
fn contains_name(root: &Path, player_name: &str) -> bool {
    let needle = player_name.replace(' ', "_");
    if needle.is_empty() {
        return false;
    }
    fn walk(dir: &Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                // `generation.zip` is the original archive, whose members legitimately carry
                // names; only the extracted tree is renamed.
                n != "generation.zip" && n.contains(needle)
            }) {
                return true;
            }
            if path.is_dir() && walk(&path, needle) {
                return true;
            }
        }
        false
    }
    walk(root, &needle)
}

/// The same bytes twice: one directory, and the second attempt says so.
#[test]
fn promoting_the_same_bytes_twice_converges() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };
    let meta = artifact::inspect(&bytes, LIMIT).expect("inspect");
    let dir = tempfile::tempdir().expect("tempdir");

    let (first, a) = artifact::promote(dir.path(), &bytes, &meta, "nonce-1").expect("first");
    let (second, b) = artifact::promote(dir.path(), &bytes, &meta, "nonce-2").expect("second");

    assert_eq!(a, Promotion::Stored);
    assert_eq!(b, Promotion::AlreadyPresent);
    assert_eq!(first.root, second.root);

    let generations = dir.path().join("generations");
    let entries: Vec<_> = std::fs::read_dir(&generations)
        .expect("read generations")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(
        !entries.iter().any(|n| n.starts_with(".tmp-")),
        "a temp directory was left behind: {entries:?}"
    );
}

/// A failed promotion leaves no directory that a row could point at.
///
/// D3's invariant in miniature: nothing may exist under a name that asserts completeness unless
/// it is complete. An unreadable member is the cheapest way to force the failure.
#[test]
fn a_failed_promotion_leaves_nothing_behind() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };
    let mut meta = artifact::inspect(&bytes, LIMIT).expect("inspect");
    meta.multidata_member = "no-such-member.archipelago".to_string();

    let dir = tempfile::tempdir().expect("tempdir");
    let err = artifact::promote(dir.path(), &bytes, &meta, "nonce-1").expect_err("should fail");
    assert!(matches!(err, artifact::StorageError::Zip { .. }), "{err:?}");

    let generations = dir.path().join("generations");
    let entries: Vec<_> = std::fs::read_dir(&generations)
        .expect("read generations")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(entries.is_empty(), "left behind: {entries:?}");
}

/// The database half of the same convergence.
#[tokio::test]
async fn indexing_the_same_generation_twice_yields_one_row() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };
    let meta = artifact::inspect(&bytes, LIMIT).expect("inspect");

    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        user::ensure_exists(&mut conn, UPLOADER)
            .await
            .expect("user");

        let first = generation::insert(&mut conn, &meta, UPLOADER)
            .await
            .expect("first insert");
        assert!(first.created);

        let second = generation::insert(&mut conn, &meta, UPLOADER)
            .await
            .expect("second insert");
        assert!(!second.created, "the second upload must not create a row");
        assert_eq!(first.id, second.id);

        // The same account twice: news once, a duplicate after. This is the value the page renders,
        // and `insert` producing it is what stops a caller indexing a generation without recording
        // who uploaded it: an upload that succeeds and then is missing from its uploader's list.
        assert!(
            first.first_for_this_user,
            "the first upload is theirs to see"
        );
        assert!(
            !second.first_for_this_user,
            "the second is a duplicate to this uploader"
        );

        // Slots are written once, not twice: the second insert returns before touching them.
        let slots = generation::slots(&mut conn, first.id).await.expect("slots");
        assert_eq!(slots.len(), meta.slots.len());

        for (row, parsed) in slots.iter().zip(&meta.slots) {
            assert_eq!(row.slot_number, parsed.slot_number);
            assert_eq!(row.player_name, parsed.player_name);
            assert_eq!(row.game, parsed.game);
            assert_eq!(row.kind, parsed.kind, "slot {}", row.slot_number);
            assert_eq!(row.patch_member, parsed.patch_member);
        }

        let stored = generation::get(&mut conn, first.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(stored.seed_name, meta.seed_name);
        assert_eq!(stored.locations, meta.locations);
        assert_eq!(stored.race_mode, meta.race_mode);
        assert_eq!(stored.has_spoiler, meta.spoiler_member.is_some());

        // `generations.slots` sizes the room's memory request, and pahoa derives its outbound
        // budget from every slot including groups, so this must be `slot_count`, not the length
        // of the connectable list. The two differ on any seed with item links.
        assert_eq!(stored.slots, meta.slot_count);

        // Indexing it put it in the uploader's list, and doing it twice left one entry there.
        let listed = generation::list_for_user(&mut conn, UPLOADER, 10)
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].generation.id, first.id);
    })
    .await;
}
