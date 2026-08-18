//! Hash `static/` into STATIC_VERSION so asset URLs bust caches on content change.
//!
//! Templates append `?v={{ base.static_version }}`. Hashing the content rather than using the
//! build time means an unchanged asset keeps its URL across rebuilds, so redeploying does not
//! needlessly invalidate every client's cache.

use std::path::Path;

use sha2::{Digest, Sha256};

fn main() {
    let dir = Path::new("static");
    println!("cargo:rerun-if-changed=static");

    let mut hasher = Sha256::new();
    let mut entries: Vec<_> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();
    // Sorted: WalkDir order is filesystem order, which is not stable across machines.
    entries.sort();

    for path in entries {
        hasher.update(path.to_string_lossy().as_bytes());
        if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(&bytes);
        }
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let digest = format!("{:x}", hasher.finalize());
    println!("cargo:rustc-env=STATIC_VERSION={}", &digest[..12]);
}
