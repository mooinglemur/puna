//! One rule, over every source file in the workspace: **a dash is not punctuation here.**
//!
//! The repository was written with a dash joining clauses in comments, doc comments, rendered
//! markup and user-facing strings, in two spellings that no single search finds. Removing them took
//! nine passes and about 1,700 edits; this is what stops the tenth.
//!
//! ## Two spellings, and searching for one is why this needed nine passes
//!
//! The same construction is written as a literal em dash and as two hyphens, and they are
//! distributed by nothing at all: whole files use one, some files use both, and one module switches
//! spelling halfway down. Every intermediate sweep that searched for the character alone reported a
//! file clean while the other spelling sat in it, and every sweep that searched for the two hyphens
//! surrounded by spaces missed the ones at a line edge, where a wrapped sentence puts them. So this
//! looks for both spellings and at both edges, because each narrowing has already cost a pass.
//!
//! ## What it deliberately does not forbid
//!
//! * **A run of three or more.** Those are section dividers, which are structure rather than
//!   punctuation, and this codebase uses them in about a dozen places.
//! * **A line whose first characters are two hyphens.** That is SQL comment syntax inside a query
//!   string, and rewriting one would change what Postgres parses.
//! * **The entity in placeholder position.** A table cell with no value renders a dash, which is a
//!   typographic convention rather than a sentence. [`prose_position`] is what tells the two apart,
//!   and it exists because the placeholder spelling hid a real one: `rooms/console.html` rendered
//!   `**release** &mdash; ok` for three passes, invisible to every search for the character because
//!   the entity is not the character.
//!
//! ## The allowlist is the record of what was kept, and it is not a suppression list
//!
//! Every entry is a decision with a reason, and the reasons fall into two kinds: a **quotation**,
//! where the dash belongs to somebody else's text and changing it would misquote, and a
//! **placeholder**, where the dash is a glyph standing in for a missing value. An entry pins the
//! line verbatim, so moving one is free and editing one fails: the allowlist cannot quietly widen
//! into cover for a new dash on a line that already had one.
//!
//! ## This file is excluded from its own scan, which is the fourth instance of that trap
//!
//! A lint has to name the thing it forbids, and four lints in this repository have shipped matching
//! their own explanatory prose and failing on a correct file. Here it is unavoidable rather than
//! incidental: the allowlist quotes the kept lines exactly, so the forbidden shapes are literally
//! present. Skipping one file is the honest version; stripping comments would not help, because the
//! shapes are in the data rather than in the commentary.

use std::path::{Path, PathBuf};

/// The em dash, spelled as an escape so this constant is not itself an instance.
const EM: char = '\u{2014}';

/// Files whose extension carries prose somebody reads.
const SCANNED: &[&str] = &["rs", "html", "js", "css"];

/// Lines that keep a dash on purpose, as `(path suffix, the line, why)`.
///
/// Pinned verbatim rather than by line number, so ordinary edits above them do not need touching
/// here, and pinned by *content* rather than by file, so a second dash appearing on one of these
/// lines is still a failure.
const KEPT: &[(&str, &str, &str)] = &[
    (
        "puna-web/src/routes/rooms.rs",
        "///     ended up. A claim link pasted into a channel unfurled as *\"Discord \u{2014} Group Chat that's all",
        "quotation: Discord's own page title, which is what the unfurl actually said",
    ),
    (
        "puna-web/tests/templates.rs",
        "/// `Friday async \u{2014} con\u{2026}` and `Friday async \u{2014} mem\u{2026}` are the same string to a reader.",
        "quotation: the old tab-title format, quoted to show what the title lint replaced",
    ),
    (
        "puna-web/templates/rooms/show.html",
        "the older `<room> &mdash; tracker` shape spent its first twenty characters on the thing every one",
        "quotation: the same old title format, in the template that stopped using it",
    ),
    (
        "puna-web/static/table.js",
        "// than per column so a column of numbers with one \"\u{2014}\" in it still sorts as numbers.",
        "quotation: names the placeholder character the sort has to tolerate",
    ),
    (
        "puna-web/static/tracker.js",
        "const dash = { text: \"\u{2014}\", class: \"hint\" };",
        "placeholder: the glyph a cell with no value renders",
    ),
    (
        "puna-web/static/tracker.js",
        "if (!contact) return { text: \"\u{2014}\", class: \"hint\", tag: r.owner.ping };",
        "placeholder: the same glyph, for a slot whose holder is withheld",
    ),
    (
        "puna-web/templates/rooms/_rule_table.html",
        "<option value=\"\">&mdash; choose &mdash;</option>",
        "placeholder: the empty-option idiom, a bracketing pair rather than a sentence",
    ),
    (
        "puna-tools/src/bin/make_generation.rs",
        "//! cargo run -p puna-tools --bin make-generation -- --slots 12 --locations 250 --out /tmp/seed.zip",
        "not a dash: cargo's argument separator, in a usage example",
    ),
    (
        "puna-tools/src/bin/room_load.rs",
        "//! cargo run -p puna-tools --bin room-load -- \\",
        "not a dash: the same separator",
    ),
];

/// Every source file in the workspace, this one excepted.
fn sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate sits in the workspace root")
        .to_path_buf();
    let mut found = Vec::new();
    walk(&root, &mut found);
    found.sort();
    found
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            // Build output and version control carry neither prose nor our authorship.
            if name != "target" && name != ".git" {
                walk(&path, out);
            }
        } else if path
            .extension()
            .is_some_and(|e| SCANNED.iter().any(|s| *s == e))
            && !path.ends_with("tests/prose.rs")
        {
            out.push(path);
        }
    }
}

/// Whether two hyphens are being used as punctuation on this line.
///
/// Three shapes, because a wrapped sentence puts the dash wherever the wrap falls: between spaces,
/// at the end of a line whose sentence continues below, and at the start of the continuation.
fn ascii_dash(line: &str) -> bool {
    let trimmed = line.trim();
    // A divider, or a longer rule. Structure, not punctuation.
    if line.contains("---") {
        return false;
    }
    // SQL comment syntax inside a query string.
    if trimmed.starts_with("--") {
        return false;
    }
    let dash = "--";
    line.contains(&format!(" {dash} ")) || trimmed.ends_with(&format!(" {dash}"))
}

/// The rendered text of a template line, with each expression standing in as a word.
///
/// Control-flow tags become a **separator** rather than nothing, because the two arms of an
/// `{% if %}` never render together: a placeholder in the `else` arm is not adjacent to the value in
/// the `if` arm, however close the two sit in the source.
fn rendered(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    loop {
        let next = ["{{", "{%", "<"]
            .iter()
            .filter_map(|open| rest.find(open).map(|at| (at, *open)))
            .min_by_key(|(at, _)| *at);
        let Some((at, open)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        let close = match open {
            "{{" => "}}",
            "{%" => "%}",
            _ => ">",
        };
        // An expression renders a value, so it stands in as a word; a control tag renders nothing
        // and separates what is on either side of it; a markup tag renders nothing at all.
        out.push_str(match open {
            "{{" => "x",
            "{%" => "\u{1}",
            _ => "",
        });
        match rest[at..].find(close) {
            Some(end) => rest = &rest[at + end + close.len()..],
            None => return out,
        }
    }
}

/// Whether the entity sits between two pieces of rendered text, which is a sentence.
///
/// A placeholder has nothing beside it: it is the whole of what its branch renders.
fn prose_position(line: &str) -> bool {
    let rendered = rendered(line);
    rendered.split('\u{1}').any(|segment| {
        let Some((before, after)) = segment.split_once("&mdash;") else {
            return false;
        };
        before.trim().chars().any(char::is_alphanumeric)
            && after.trim().chars().any(char::is_alphanumeric)
    })
}

fn allowed(path: &Path, line: &str) -> bool {
    let path = path.to_string_lossy().replace('\\', "/");
    KEPT.iter()
        .any(|(suffix, kept, _)| path.ends_with(suffix) && line.trim() == kept.trim())
}

/// **No comment, string or rendered line in the workspace uses a dash as punctuation.**
///
/// Both spellings, at both line edges, plus the entity in prose position. See the module docs for
/// what is deliberately exempt and why the allowlist is a record rather than a suppression.
#[test]
fn prose_does_not_use_a_dash_as_punctuation() {
    let sources = sources();
    let mut offenders = Vec::new();

    for path in &sources {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let extension = path.extension().unwrap_or_default().to_string_lossy();
        for (number, line) in text.lines().enumerate() {
            if allowed(path, line) {
                continue;
            }
            let at = format!("{}:{}", path.display(), number + 1);
            if line.contains(EM) {
                offenders.push(format!("{at}: em dash\n    {}", line.trim()));
            } else if ascii_dash(line) {
                offenders.push(format!("{at}: two hyphens\n    {}", line.trim()));
            } else if extension == "html" && prose_position(line) {
                offenders.push(format!("{at}: dash entity in prose\n    {}", line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a dash is being used as punctuation. Use a colon where the second half explains the \
         first, a full stop between two independent clauses, a comma or a conjunction for an \
         aside, or parentheses where a dashed pair brackets an interjection that has commas in \
         it. If it is a quotation or a placeholder, add it to KEPT with its reason.\n\n{}",
        offenders.join("\n")
    );

    // A source lint that scans nothing passes. This walks the whole workspace, so the floor is
    // high enough that a broken walk cannot look like a clean repository.
    assert!(
        sources.len() > 100,
        "only {} source files scanned: this lint is no longer looking at anything",
        sources.len()
    );
}
