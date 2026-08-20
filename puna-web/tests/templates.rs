//! A lint over the template sources.
//!
//! `askama.toml` sets `whitespace = "suppress"`, which strips whitespace adjacent to every tag. The
//! escape is `{{+ ... }}`, and it **preserves** whitespace rather than inserting any — so
//! `as {{+ name }}` is right and `as{{+ name }}` renders `asTroy`.
//!
//! The second form looks like it does the same thing, and on 2026-08-20 **every** use of `{{+` in
//! this crate was that shape: the space had been deleted at the same time the `+` was added. It
//! reached production on the home page. Nothing about it is visible in a diff or a compile, and a
//! render test only catches the one string it happens to assert — so the guard is a lint over the
//! sources, which catches every instance including ones nobody has written yet.

use std::path::{Path, PathBuf};

fn templates() -> Vec<PathBuf> {
    fn walk(dir: &Path, into: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read the templates directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(&path, into);
            } else if path.extension().is_some_and(|e| e == "html") {
                into.push(path);
            }
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    let mut found = Vec::new();
    walk(&root, &mut found);
    assert!(!found.is_empty(), "no templates found under {root:?}");
    found
}

/// `{{+` with no whitespace in front of it preserves nothing, so it is always a mistake: either the
/// space belongs there, or the `+` does not.
#[test]
fn a_whitespace_preserving_tag_has_whitespace_to_preserve() {
    let mut offenders = Vec::new();

    for path in templates() {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
        // Comments are blanked rather than skipped by line, because `base.html` documents the wrong
        // form ON PURPOSE and a comment spans lines. Blanking preserves line numbers, so the ones
        // reported below still point at the real file.
        let source = blank_comments(&raw);

        for (number, line) in source.lines().enumerate() {
            for (at, _) in line.match_indices("{{+").chain(line.match_indices("{%+")) {
                let preceding = line[..at].chars().next_back();
                match preceding {
                    // Start of line: the newline before it is the whitespace being preserved.
                    None => {}
                    Some(c) if c.is_whitespace() => {}
                    Some(c) => offenders.push(format!(
                        "{}:{}: `{}{{{{+` preserves nothing -- add the space or drop the `+`",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        number + 1,
                        c
                    )),
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "whitespace-preserving tags with no whitespace to preserve:\n  {}",
        offenders.join("\n  ")
    );
}

/// Replace every `{# ... #}` span with spaces, keeping newlines so line numbers survive.
///
/// Askama comments do not nest, so a scan for the next `#}` is the whole job.
fn blank_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    while i < bytes.len() {
        if source[i..].starts_with("{#") {
            let end = source[i..]
                .find("#}")
                .map_or(bytes.len(), |offset| i + offset + 2);
            for c in source[i..end].chars() {
                out.push(if c == '\n' { '\n' } else { ' ' });
            }
            i = end;
        } else {
            let c = source[i..].chars().next().expect("a character");
            out.push(c);
            i += c.len_utf8();
        }
    }

    out
}
