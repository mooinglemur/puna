//! Address embedding against a real patch, taken out of a real generation zip.
//!
//! The unit tests in `artifact::patch` build their own archives, which means they test the code
//! against its author's idea of what a patch looks like. This one takes the genuine article --
//! whatever compression, member layout and manifest keys the game's own tooling produced -- and
//! asserts the only thing that must be true of it: **the server field changes and nothing else
//! does.** A patch is a file a game will open, so "nothing else" is the whole contract.
//!
//! Gated on `PUNA_TEST_GENERATION_ZIP`, like the ingest suite, because a real zip is tens of
//! megabytes and carries real players' names.

use std::io::{Cursor, Read};

use puna_core::artifact;

fn fixture() -> Option<Vec<u8>> {
    let path = std::env::var("PUNA_TEST_GENERATION_ZIP").ok()?;
    let path = match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => path.clone(),
        },
        None => path.clone(),
    };
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) => panic!("PUNA_TEST_GENERATION_ZIP={path} could not be read: {e}"),
    }
}

/// Every member of a zip, by name, decompressed.
fn members(zip: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip)).expect("a zip");
    let mut out = std::collections::BTreeMap::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).expect("a member");
        let name = file.name().to_string();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).expect("read");
        out.insert(name, contents);
    }
    out
}

#[test]
fn a_real_patch_keeps_everything_but_its_server_field() {
    // Skipped, never failed, when the fixture is absent -- and deliberately NOT tied to
    // `PUNA_REQUIRE_DB_TESTS`, which is about Postgres. CI has a database and does not have a
    // generation zip: real ones are tens of megabytes and carry real players' names, so they stay
    // out of the repository. Coupling the two turns "CI has no fixture" into a red pipeline.
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping the real-patch test");
        return;
    };

    // Find a patch inside the generation: the members whose extension starts with `ap`, which is
    // the convention every Archipelago patch format follows.
    let mut archive = zip::ZipArchive::new(Cursor::new(&bytes)).expect("the generation zip");
    let patch_names: Vec<String> = archive
        .file_names()
        .filter(|name| {
            name.rsplit_once('.')
                .is_some_and(|(_, ext)| ext.to_ascii_lowercase().starts_with("ap"))
                && !name.ends_with(".archipelago")
        })
        .map(str::to_string)
        .collect();

    let Some(name) = patch_names.first().cloned() else {
        eprintln!("this generation has no patch members; skipping");
        return;
    };

    let original = {
        let mut file = archive.by_name(&name).expect("the patch member");
        let mut out = Vec::new();
        file.read_to_end(&mut out).expect("read");
        out
    };

    // A patch is itself a zip; if this one is not, the passthrough path is what is under test and
    // the assertion below still holds.
    let embedded =
        artifact::embed_server(original.clone(), "rooms.example.com", 41234).expect("embed");

    let Ok(before) = zip::ZipArchive::new(Cursor::new(&original)) else {
        assert_eq!(
            embedded, original,
            "a non-zip patch must be served unchanged"
        );
        return;
    };
    let has_manifest = before.file_names().any(|n| n == "archipelago.json");
    if !has_manifest {
        assert_eq!(embedded, original, "a patch with no manifest is unchanged");
        return;
    }

    let before = members(&original);
    let after = members(&embedded);

    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "the member list must not change"
    );

    for (name, contents) in &before {
        if name == "archipelago.json" {
            continue;
        }
        assert_eq!(
            after.get(name),
            Some(contents),
            "{name} is a game's own data and must come back bit for bit"
        );
    }

    let original_manifest: serde_json::Value =
        serde_json::from_slice(&before["archipelago.json"]).expect("the manifest is json");
    let rewritten: serde_json::Value =
        serde_json::from_slice(&after["archipelago.json"]).expect("the rewritten manifest is json");

    assert_eq!(rewritten["server"], "rooms.example.com:41234");

    // Every other key of a real manifest -- `game`, `player`, `patch_file_ending`,
    // `compatible_version`, whatever this game happens to carry -- survives untouched. This is the
    // assertion that would catch a rewrite that reconstructed the manifest instead of editing it.
    let original_object = original_manifest.as_object().expect("an object");
    let rewritten_object = rewritten.as_object().expect("an object");
    assert_eq!(
        original_object.len(),
        rewritten_object.len(),
        "no key may be added or dropped"
    );
    for (key, value) in original_object {
        if key == "server" {
            continue;
        }
        assert_eq!(rewritten_object.get(key), Some(value), "{key} changed");
    }

    eprintln!(
        "embedded into {name}: {} members, {} bytes -> {} bytes",
        before.len(),
        original.len(),
        embedded.len()
    );
}
