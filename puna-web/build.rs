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

    println!("cargo:rustc-env=PUNA_BUILD_REV={}", build_rev());
}

/// Which commit this binary was built from, as the footer reports it.
///
/// **`CI_COMMIT_SHORT_SHA` first, because that is the string the IMAGE TAG is made of.** The
/// pipeline pushes `:sha-$CI_COMMIT_SHORT_SHA` and compiles in an earlier job of the same pipeline,
/// so taking the same variable makes the footer and the tag agree by construction rather than by
/// two things happening to be derived from one commit. It also needs no change to CI at all.
///
/// That matters more than it sounds: GitLab's short sha is **eight** characters and `git rev-parse
/// --short` defaults to seven, so a footer built the obvious way would print a string that looks
/// like the tag, differs from it by one character, and sends somebody comparing the two to the
/// registry to find nothing.
///
/// Falling back to git for a local build, and to `dev` where there is neither, which is what a
/// build from a tarball or a vendored source drop gets. Never a panic: a footer is not worth
/// failing a build over.
fn build_rev() -> String {
    // Without this, the cargo cache is keyed on the branch and a second commit on one branch would
    // reuse an object baked with the first commit's sha: a footer naming a build that is not the
    // one running, which is worse than naming none.
    println!("cargo:rerun-if-env-changed=CI_COMMIT_SHORT_SHA");

    if let Ok(sha) = std::env::var("CI_COMMIT_SHORT_SHA")
        && !sha.is_empty()
    {
        return sha;
    }

    std::process::Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "dev".to_string())
}
