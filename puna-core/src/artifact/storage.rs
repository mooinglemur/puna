//! Putting a validated generation on disk, content-addressed.
//!
//! The layout, which the web tier owns and mounts read-write at `generations/` (and nothing else,
//! by `subPath`, it cannot reach room state):
//!
//! ```text
//! generations/<sha256-hex>/
//!   ├── generation.zip      the original, as received
//!   ├── seed.archipelago    extracted multidata
//!   ├── patches/<slot>.<ext>
//!   └── spoiler.txt         if the zip carried one
//! ```
//!
//! ## The rename is the whole design
//!
//! Everything is written into `generations/.tmp-<nonce>/` and then `rename`d onto the final
//! `<sha256-hex>` name. That single atomic step gives three properties at once:
//!
//!   * **Dedup and idempotence are the same mechanism.** The directory name IS the content hash,
//!     so uploading a zip twice converges on one directory rather than needing a check-then-write
//!     that two concurrent uploads could interleave.
//!   * **A partially written generation is never visible.** A reader either sees a complete
//!     directory or no directory. There is no window where `seed.archipelago` exists but the
//!     patches do not, which a naive "mkdir then fill" would have.
//!   * **A crash leaves only garbage that names nothing.** An abandoned `.tmp-*` is referenced by
//!     no row and swept after an hour by the orchestrator's slow lane.
//!
//! `EEXIST` on the rename is therefore **success, not a conflict**: somebody else (another
//! replica, or this user's double-click) already promoted these exact bytes. See
//! [`Promotion::AlreadyPresent`].
//!
//! ## Why the web tier does this synchronously
//!
//! A bad zip becomes a 400 on the upload form rather than a room whose pod crashloops minutes
//! later with the reason buried in a container log. That is worth the request latency.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use crate::artifact::GenerationMeta;

/// What happened when a validated zip was promoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promotion {
    /// These bytes were not on disk and now are.
    Stored,
    /// A directory for this hash already existed, so nothing was written.
    ///
    /// Not an error. The content is identical by definition (the name is its hash) so the
    /// caller proceeds exactly as it would have.
    AlreadyPresent,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("reading {member} out of the archive: {source}")]
    Zip {
        member: String,
        #[source]
        source: zip::result::ZipError,
    },

    /// A member's path escapes the directory it is being written into.
    ///
    /// Zip archives can carry `../` and absolute paths, and a naive extractor will happily write
    /// through them. Puna's own naming means this cannot arise from a well-formed generation, so
    /// hitting it is evidence of a crafted archive and the upload is refused outright.
    #[error("archive member {member} escapes the extraction directory; refusing the upload")]
    UnsafeMember { member: String },
}

fn io<T>(result: std::io::Result<T>, context: impl Into<String>) -> Result<T, StorageError> {
    result.map_err(|source| StorageError::Io {
        context: context.into(),
        source,
    })
}

/// Where a generation's files live, once promoted.
#[derive(Debug, Clone)]
pub struct GenerationPaths {
    pub root: PathBuf,
}

impl GenerationPaths {
    /// The directory for one content hash under `<data_dir>/generations/`.
    pub fn new(data_dir: &Path, sha256: &[u8; 32]) -> Self {
        Self {
            root: data_dir.join("generations").join(hex(sha256)),
        }
    }

    pub fn archive(&self) -> PathBuf {
        self.root.join("generation.zip")
    }

    pub fn seed(&self) -> PathBuf {
        self.root.join("seed.archipelago")
    }

    pub fn spoiler(&self) -> PathBuf {
        self.root.join("spoiler.txt")
    }

    /// A slot's patch, named by slot number rather than by the member's own name.
    ///
    /// Renaming on extraction is deliberate: the member name carries a player's name, and this
    /// path is what a download handler builds from a URL parameter. Deriving it from an integer
    /// means no user-controlled text ever reaches the filesystem.
    pub fn patch(&self, slot_number: i32, extension: &str) -> PathBuf {
        self.root
            .join("patches")
            .join(format!("{slot_number}.{extension}"))
    }

    pub fn exists(&self) -> bool {
        self.root.is_dir()
    }
}

/// A generation's directory name.
///
/// Takes exactly 32 bytes rather than a slice, because a content address that is not a whole digest
/// is not a content address. The formatting itself is [`crate::hash::hex`].
pub fn hex(bytes: &[u8; 32]) -> String {
    crate::hash::hex(bytes)
}

/// Write a validated generation into `<data_dir>/generations/<sha256>/`.
///
/// `meta` must have come from [`crate::artifact::inspect`] over these same `bytes`; the hash in
/// it is what names the directory, so passing a mismatched pair would file the content under the
/// wrong name.
pub fn promote(
    data_dir: &Path,
    bytes: &[u8],
    meta: &GenerationMeta,
    nonce: &str,
) -> Result<(GenerationPaths, Promotion), StorageError> {
    let paths = GenerationPaths::new(data_dir, &meta.sha256);

    // A cheap pre-check. Not the guarantee (the rename below is) but it turns the common
    // re-upload into one `stat` rather than a full extraction that is then thrown away.
    if paths.exists() {
        return Ok((paths, Promotion::AlreadyPresent));
    }

    let generations = data_dir.join("generations");
    io(
        std::fs::create_dir_all(&generations),
        format!("creating {}", generations.display()),
    )?;

    let tmp = generations.join(format!(".tmp-{nonce}"));
    // A leftover from a crashed attempt with the same nonce would otherwise poison this one.
    let _ = std::fs::remove_dir_all(&tmp);
    io(
        std::fs::create_dir(&tmp),
        format!("creating {}", tmp.display()),
    )?;

    // Anything that fails from here leaves the tmp directory behind rather than a half-promoted
    // generation, which the hourly sweep removes. That is the failure mode worth having.
    let result = fill(&tmp, bytes, meta);
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }

    io(sync_dir(&tmp), format!("fsyncing {}", tmp.display()))?;

    match std::fs::rename(&tmp, &paths.root) {
        Ok(()) => {
            // fsync the parent so the rename itself survives a crash, not just the file contents.
            io(
                sync_dir(&generations),
                format!("fsyncing {}", generations.display()),
            )?;
            Ok((paths, Promotion::Stored))
        }
        // Somebody promoted these exact bytes while this upload was extracting. The directory
        // name is the content hash, so their copy and this one are the same thing.
        Err(e) if is_already_present(&e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Ok((paths, Promotion::AlreadyPresent))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(StorageError::Io {
                context: format!("renaming {} to {}", tmp.display(), paths.root.display()),
                source: e,
            })
        }
    }
}

/// Does this rename failure mean "the destination is already there"?
///
/// Linux reports `ENOTEMPTY` rather than `EEXIST` when renaming a directory onto a populated one,
/// and both are the same situation for us: somebody won the race.
fn is_already_present(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
    )
}

/// Extract everything into an already-created directory.
fn fill(dir: &Path, bytes: &[u8], meta: &GenerationMeta) -> Result<(), StorageError> {
    write_file(&dir.join("generation.zip"), bytes)?;

    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|source| StorageError::Zip {
            member: "<archive>".to_string(),
            source,
        })?;

    let multidata = read_member(&mut archive, &meta.multidata_member)?;
    write_file(&dir.join("seed.archipelago"), &multidata)?;

    if let Some(spoiler) = &meta.spoiler_member {
        let contents = read_member(&mut archive, spoiler)?;
        write_file(&dir.join("spoiler.txt"), &contents)?;
    }

    let patches = dir.join("patches");
    io(
        std::fs::create_dir(&patches),
        format!("creating {}", patches.display()),
    )?;

    for slot in &meta.slots {
        let Some(member) = &slot.patch_member else {
            continue;
        };
        let contents = read_member(&mut archive, member)?;
        let extension = patch_extension(member);
        // Slot number, not the member's name: see `GenerationPaths::patch`.
        let target = patches.join(format!("{}.{extension}", slot.slot_number));
        guard_inside(&patches, &target, member)?;
        write_file(&target, &contents)?;
    }

    Ok(())
}

/// The extension a patch member should keep, sanitized.
///
/// Clients dispatch on the extension, so it has to survive, but it reaches the filesystem, so
/// anything that is not plainly alphanumeric is replaced rather than trusted. `bin` is the
/// fallback for a member with no usable extension at all.
///
/// **Public because the download handler has to derive the same name the writer used.** Two copies
/// of this rule would diverge on exactly the inputs it exists for, and the symptom would be a 404 on
/// a patch that is sitting on disk under a slightly different name.
pub fn patch_extension(member: &str) -> String {
    let raw = member
        .rsplit('/')
        .next()
        .unwrap_or(member)
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("");

    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect::<String>()
        .to_ascii_lowercase();

    if cleaned.is_empty() {
        "bin".to_string()
    } else {
        cleaned
    }
}

/// Refuse a path that leaves its directory.
///
/// Puna derives every extracted name itself, so this cannot fire on a well-formed upload. It is
/// here because "the names are ours" is a property that holds until someone changes the naming
/// code, and the consequence of it not holding is an arbitrary file write.
fn guard_inside(root: &Path, target: &Path, member: &str) -> Result<(), StorageError> {
    if target.parent() != Some(root) || target.components().any(|c| c.as_os_str() == "..") {
        return Err(StorageError::UnsafeMember {
            member: member.to_string(),
        });
    }
    Ok(())
}

fn read_member(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    member: &str,
) -> Result<Vec<u8>, StorageError> {
    let mut file = archive
        .by_name(member)
        .map_err(|source| StorageError::Zip {
            member: member.to_string(),
            source,
        })?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    io(file.read_to_end(&mut buf), format!("reading {member}"))?;
    Ok(buf)
}

/// Write and fsync one file.
///
/// The fsync matters: a rename is atomic with respect to the directory entry, not to the contents
/// of the files inside it. Without this, a crash could leave a correctly named directory holding
/// empty files, which is worse than no directory, because the name asserts completeness.
fn write_file(path: &Path, contents: &[u8]) -> Result<(), StorageError> {
    let mut file = io(
        std::fs::File::create(path),
        format!("creating {}", path.display()),
    )?;
    io(
        file.write_all(contents),
        format!("writing {}", path.display()),
    )?;
    io(file.sync_all(), format!("fsyncing {}", path.display()))?;
    Ok(())
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_extensions_are_kept_but_sanitized() {
        assert_eq!(patch_extension("AP_1_P3_Name.apsms"), "apsms");
        assert_eq!(patch_extension("AP_1_P3_Name.APZ3"), "apz3");
        assert_eq!(patch_extension("dir/AP_1_P3_Name.chunky"), "chunky");
        // No extension, and names that would be unpleasant on a filesystem.
        assert_eq!(patch_extension("AP-20240224"), "bin");
        assert_eq!(patch_extension("x."), "bin");
        assert_eq!(patch_extension("x../../../etc/passwd"), "bin");
        assert_eq!(patch_extension("x.ap z3"), "apz3");
    }

    #[test]
    fn hex_is_lowercase_and_full_width() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0x0a;
        bytes[31] = 0xff;
        let s = hex(&bytes);
        assert_eq!(s.len(), 64);
        assert!(s.starts_with("0a"));
        assert!(s.ends_with("ff"));
    }

    #[test]
    fn a_member_escaping_its_directory_is_refused() {
        let root = Path::new("/data/generations/abc/patches");
        assert!(guard_inside(root, &root.join("3.apsms"), "ok").is_ok());
        assert!(guard_inside(root, Path::new("/etc/passwd"), "bad").is_err());
        assert!(guard_inside(root, &root.join("../seed.archipelago"), "bad").is_err());
    }
}
