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

/// The other half of the same trap: a space between rendered text and a tag is **eaten**.
///
/// The first lint catches `word{{+ x }}`, where the `+` preserves nothing. This catches the case
/// with no `+` at all — `{{ count }} rooms` renders `4rooms`, and `running {% if %}` renders
/// `runningsha-abc`. Both shipped: the drift label and the configured-image line on
/// `/admin/rooms`, within an hour of each other, by someone who had just fixed the first kind.
///
/// **A render test cannot cover this**, which is why it is a source lint. It only catches the exact
/// string it asserts, and these are one-word joins scattered through markup nobody re-reads.
///
/// The rule: whitespace adjacent to a tag survives only with an explicit `+` on that side. So this
/// flags whitespace with **rendered text on both sides** of it, which is the only case where the
/// loss is visible. Whitespace between two tags (`{% endif %}\n{% if %}`) is not flagged: nothing
/// renders there. Nor is whitespace against markup (`>\n{% if %}`, `{{ x }}\n<td>`), where HTML
/// collapses it anyway.
///
/// **A newline in the run means it is not flagged either**, and that exclusion is what makes this
/// usable rather than noisy. The common harmless shape is a branch whose whole body is on its own
/// line:
///
/// ```text
/// {% else if row.complete() %}
/// cached
/// {% else %}
/// ```
///
/// Here the stripped whitespace is layout, the word is the entire cell, and nothing joins to
/// anything. A run with no newline is different in kind: it is a space someone typed *between two
/// words on one line*, and losing it is always visible. Twelve of the former and fourteen of the
/// latter existed when this lint was written; only the latter were bugs.
#[test]
fn whitespace_between_text_and_a_tag_is_preserved_explicitly() {
    let mut offenders = Vec::new();

    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        // The last two components, because there are two `show.html` files and a bare basename
        // sent me to the wrong one while writing this.
        let name = label(&path);

        for (at, opener) in tag_positions(&source) {
            // Text, then whitespace, then this tag: the whitespace before it is eaten.
            let before = source[..at].trim_end_matches([' ', '\t', '\n', '\r']);
            let run = &source[before.len()..at];
            let preserved = source[at..].starts_with(&format!("{opener}+"));

            if run.is_empty() || preserved || !before.ends_with(is_rendered_text) {
                continue;
            }
            // Whitespace before a block terminator is trailing content rather than a word join:
            // nothing follows it inside the branch for the text to run together with.
            if run.contains('\n')
                && (terminates_a_block(&source[at..])
                    || !line_containing(&source, before.len() - 1).contains('<'))
            {
                continue;
            }
            if let Some(line) = line_of(&source, at) {
                offenders.push(format!(
                    "{name}:{line}: text before `{opener}` loses its space -- write `{opener}+`"
                ));
            }
        }

        for (end, closer) in close_positions(&source) {
            let after = source[end..].trim_start_matches([' ', '\t', '\n', '\r']);
            let text_at = source.len() - after.len();
            let run = &source[end..text_at];
            // `+` rides INSIDE the closer -- `+}}` -- so it is the character before it, not after.
            let preserved = source[..end - closer.len()].ends_with('+');

            if run.is_empty() || preserved || !after.starts_with(is_rendered_text) {
                continue;
            }
            if run.contains('\n') && !line_containing(&source, text_at).contains('<') {
                continue;
            }
            if let Some(line) = line_of(&source, end) {
                offenders.push(format!(
                    "{name}:{line}: text after `{closer}` loses its space -- write `+{closer}`"
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "whitespace adjacent to a tag is stripped, so these render run together:\n  {}",
        offenders.join("\n  ")
    );
}

/// Content that actually renders, as opposed to markup or another tag.
///
/// `>` and `<` are excluded because HTML collapses whitespace around a tag boundary anyway, and
/// `{`/`}` because whitespace between two template tags renders nothing either way.
///
/// Whitespace itself is excluded too, and leaving it out was the bug in this lint's first draft:
/// the runs are trimmed of spaces and tabs only, so a tag indented on its own line leaves a `\n`
/// as the neighbouring character. Counting that as text flagged every indented `{% if %}` in the
/// crate.
fn is_rendered_text(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '>' | '<' | '{' | '}')
}

fn line_of(source: &str, at: usize) -> Option<usize> {
    Some(source.get(..at)?.matches('\n').count() + 1)
}

/// The whole source line containing `at`.
///
/// Used to tell a prose line running into a tag (`<p>Rooms should be running` + newline + `{% if
/// %}`) from a branch body sitting alone on its line (`cached`). The first joins to whatever the
/// tag emits; the second is the entire cell and joins to nothing.
fn line_containing(source: &str, at: usize) -> &str {
    let start = source[..at].rfind('\n').map_or(0, |n| n + 1);
    let end = source[at..].find('\n').map_or(source.len(), |n| at + n);
    &source[start..end]
}

/// `{% endif %}`, `{% else %}` and friends -- tags that close or divide a block.
fn terminates_a_block(tag: &str) -> bool {
    let body = tag
        .trim_start_matches(['{', '%', '+'])
        .trim_start()
        .trim_start_matches('-')
        .trim_start();
    body.starts_with("end") || body.starts_with("else") || body.starts_with("when")
}

/// `tracker/show.html` rather than `show.html`. There are two of each name under `templates/`, and
/// a bare basename points at whichever one the reader assumes.
fn label(path: &Path) -> String {
    let mut parts: Vec<String> = path
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.reverse();
    parts.join("/")
}

fn tag_positions(source: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for opener in ["{{", "{%"] {
        found.extend(source.match_indices(opener).map(|(at, _)| (at, opener)));
    }
    found
}

/// Byte offsets just past each closing delimiter.
fn close_positions(source: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for closer in ["}}", "%}"] {
        found.extend(
            source
                .match_indices(closer)
                .map(|(at, _)| (at + closer.len(), closer)),
        );
    }
    found
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
