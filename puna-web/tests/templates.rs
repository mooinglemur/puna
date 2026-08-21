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

/// A control with no words carries **both** `title` and `aria-label`, which are not alternatives.
///
/// `aria-label` names the control for assistive technology and renders nothing on screen; `title`
/// is the hover tooltip a pointer user gets. A button whose entire content is an `<svg>` shows no
/// words at all, so with only the first it is a picture nobody can identify by pointing at it.
///
/// **This was reported twice.** M19b gave the four glyph controls in `rooms/show.html` a `title`
/// after the gap was noticed there; `rooms/panel.html`'s address copy button was missed and was
/// reported the same way the next day. Neither was visible to anything else here — the markup is
/// valid, the attribute that *is* present is spelled correctly, and the control works.
#[test]
fn a_glyph_only_control_names_itself_twice() {
    let mut offenders = Vec::new();
    let mut examined = 0;

    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        let name = label(&path);

        // `summary` belongs here with the other two: `<details>` is this codebase's default toggle
        // because it needs no script, so a glyph-only summary is an ordinary control rather than an
        // exotic one -- the room page's rename pencil is exactly that.
        for element in ["button", "a", "summary"] {
            for (at, open, content) in elements(&source, element) {
                if renders_text(content) {
                    continue;
                }
                examined += 1;
                for attribute in ["title=", "aria-label="] {
                    if !open.contains(attribute) {
                        let line = line_of(&source, at).unwrap_or_default();
                        offenders.push(format!(
                            "{name}:{line}: <{element}> renders no words and has no `{attribute}`"
                        ));
                    }
                }
            }
        }
    }

    // A source lint is the easiest kind to write vacuously, so say how much it must have seen. Seventeen
    // glyph controls exist today; a change that leaves none is a change this lint stopped guarding.
    assert!(
        examined >= 17,
        "only {examined} glyph-only controls found -- this lint is no longer looking at anything"
    );
    assert!(
        offenders.is_empty(),
        "a control with no words needs a hover tooltip AND an accessible name:\n  {}",
        offenders.join("\n  ")
    );
}

/// Every `<name ...>...</name>` in the source, as its offset, its opening tag and its content.
///
/// No element here nests inside another of its own kind, so matching the next closing tag is the
/// whole job.
fn elements<'a>(source: &'a str, name: &str) -> Vec<(usize, &'a str, &'a str)> {
    let (open, close) = (format!("<{name}"), format!("</{name}>"));
    let mut found = Vec::new();
    let mut i = 0;

    while let Some(offset) = source[i..].find(&open) {
        let at = i + offset;
        i = at + open.len();
        // `<a` must not match `<abbr`, so the name has to end where the tag says it does.
        if !matches!(source[i..].chars().next(), Some(c) if c.is_whitespace() || c == '>') {
            continue;
        }
        let Some(tag_end) = source[at..].find('>').map(|n| at + n + 1) else {
            break;
        };
        let Some(end) = source[tag_end..].find(&close).map(|n| tag_end + n) else {
            break;
        };
        found.push((at, &source[at..tag_end], &source[tag_end..end]));
        i = end + close.len();
    }

    found
}

/// Whether an element's content puts any words on screen.
///
/// An `<svg>` is dropped whole — it *is* the glyph, not a label for it — as are markup tags and
/// control-flow tags, which render nothing themselves. An expression `{{ ... }}` counts as text,
/// because whatever it interpolates is something the reader can see and read the control by.
fn renders_text(content: &str) -> bool {
    let mut rendered = String::new();
    let mut i = 0;

    while i < content.len() {
        let rest = &content[i..];
        if rest.starts_with("<svg") {
            i += rest
                .find("</svg>")
                .map_or(rest.len(), |n| n + "</svg>".len());
        } else if rest.starts_with("{%") {
            i += rest.find("%}").map_or(rest.len(), |n| n + 2);
        } else if rest.starts_with("{{") {
            rendered.push('x');
            i += rest.find("}}").map_or(rest.len(), |n| n + 2);
        } else if rest.starts_with('<') {
            i += rest.find('>').map_or(rest.len(), |n| n + 1);
        } else {
            let c = rest.chars().next().expect("a character");
            rendered.push(c);
            i += c.len_utf8();
        }
    }

    !rendered.trim().is_empty()
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

/// **The theme selector's contract, which spans three files and fails silently in all of them.**
///
/// Choosing a theme is one property: `theme.js` writes `data-theme` on `<html>`, and `puna.css`
/// turns that into `color-scheme`, which every `light-dark()` token resolves against. The three
/// buttons name their choice in `data-set`.
///
/// Break any half of that and **the control keeps working**: the button still highlights, the
/// choice still persists across a reload, and the page never changes color. Nothing errors, nothing
/// logs, and the only way to notice is to look at it — which is how a theme switcher gets shipped
/// broken and stays that way.
///
/// So each side is read out of its own file and checked against the others.
#[test]
fn the_theme_selector_agrees_across_markup_script_and_stylesheet() {
    let script = std::fs::read_to_string(source("static/theme.js")).expect("theme.js");
    let css = std::fs::read_to_string(source("static/css/puna.css")).expect("puna.css");
    let base = std::fs::read_to_string(source("templates/base.html")).expect("base.html");

    // The attribute the script writes is the attribute the stylesheet keys on. Written as
    // `dataset.theme` in JavaScript and `[data-theme=` in CSS, so neither spelling can be grepped
    // for in the other file -- which is exactly why this drifts unnoticed.
    assert!(
        script.contains("documentElement.dataset.theme"),
        "theme.js no longer writes the attribute the stylesheet reads"
    );

    for choice in ["light", "dark"] {
        assert!(
            css.contains(&format!(
                ":root[data-theme=\"{choice}\"] {{ color-scheme: {choice}; }}"
            )),
            "puna.css does not turn data-theme={choice} into a color-scheme, so choosing it \
             would change nothing"
        );
        assert!(
            base.contains(&format!("data-set=\"{choice}\"")),
            "no button offers {choice}"
        );
    }

    // Following the system is the third state and is the ABSENCE of a stored value, so the
    // stylesheet marks it active with `:not([data-theme])` rather than a value of its own. A
    // `[data-theme="system"]` rule would never match anything the script writes.
    assert!(
        base.contains("data-set=\"system\""),
        "no button offers following the system"
    );
    assert!(
        css.contains(":root:not([data-theme]) .theme button[data-set=\"system\"]"),
        "nothing marks the follow-the-system button as the active one"
    );
    assert!(
        !css.contains("[data-theme=\"system\"]"),
        "`system` is the absence of the attribute, so a rule keyed on that value is dead"
    );

    // The bare `:root` must keep `light dark`: a reader who has chosen nothing follows their
    // system, which this page has done since M10 and which the override must not take away.
    assert!(
        css.contains("color-scheme: light dark;"),
        "the default stopped following the system"
    );

    // Revealed by a class, because without scripting the control cannot work at all.
    assert!(script.contains("classList.add(\"js-theme\")"));
    assert!(css.contains(".theme { display: none; }") && css.contains(".js-theme .theme {"));

    // **Not deferred.** A deferred script runs after parsing, so the page would paint in the system
    // theme and snap to the chosen one -- a white flash on every navigation for somebody who picked
    // dark. This is the assertion that keeps somebody from "tidying" it in with the others.
    //
    // Anchored on `<script` AND the filename together, not on the filename alone: the comment
    // above the tag explains why it is not deferred and therefore *mentions* `static/theme.js`, so
    // a search for the name finds prose first and this passes with the tag mutated. It did, until
    // a mutation caught it. Same shape as the dispatcher's ordering lint.
    let tag = base
        .lines()
        .find(|line| line.contains("<script") && line.contains("theme.js"))
        .expect("base.html loads theme.js with a script tag");
    assert!(
        !tag.contains("defer") && !tag.contains("async"),
        "theme.js must block, or the theme arrives after the first paint: {tag}"
    );
}

/// A path inside the crate, for reading a source file a test asserts against.
fn source(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// **Viewing the site as somebody else must be read-only, and this asserts WHERE that is decided.**
///
/// The rule lives in the `Session` request guard rather than in `LoggedInSession`, and that is not
/// a stylistic choice: `POST /room/<id>/start` takes a plain `Session`, because D8 lets an
/// anonymous visitor start an idle room. A check one rung up would leave exactly that route open,
/// and the symptom would be a room started by somebody who did not start it — invisible in the
/// audit trail, which would name the person being viewed.
///
/// Asserted over the source because there is no way to assert "no route was forgotten" from a
/// test that exercises routes: the property is about the guard every other guard composes on.
#[test]
fn viewing_as_somebody_is_read_only_at_the_base_guard() {
    // **Comments stripped first.** Prose about a rule contains the rule's own identifiers -- the
    // `LoggedInSession` guard's comment explains why it does not call `from_request_sync`, and
    // naming it there was enough to trip the negative assertion below. Third time in this codebase
    // that a source lint has matched its own explanation; see `puna-silent-breakage`.
    let auth = code_only(&std::fs::read_to_string(source("src/auth.rs")).expect("auth.rs"));

    // The refusal, inside the `Session` impl rather than any other. Split there so a copy of this
    // check living only in `LoggedInSession` cannot satisfy it.
    let base = auth
        .split_once("impl<'r> FromRequest<'r> for Session {")
        .expect("the Session guard exists")
        .1
        .split_once("\n}")
        .expect("it ends")
        .0;

    assert!(
        base.contains("view_as.is_some()") && base.contains("Method::Get"),
        "the base session guard no longer refuses writes while viewing as somebody: a write route \
         taking a plain `Session` -- POST /room/<id>/start does -- would be reachable"
    );

    // `LoggedInSession` must go THROUGH that guard rather than around it. Calling the sync
    // constructor here instead would quietly exempt every authenticated write route from the rule
    // above, and nothing else would look different.
    let logged_in = auth
        .split_once("impl<'r> FromRequest<'r> for LoggedInSession {")
        .expect("the LoggedInSession guard exists")
        .1
        .split_once("\n}")
        .expect("it ends")
        .0;
    assert!(
        logged_in.contains("request.guard::<Session>()"),
        "LoggedInSession bypasses the Session guard, so the read-only rule does not reach it"
    );
    assert!(
        !logged_in.contains("from_request_sync"),
        "LoggedInSession reads the cookie directly, which skips the read-only refusal"
    );

    // Exactly one route may write while impersonating, and it is the way out. It takes no session
    // guard at all -- it could not be reached through one -- so the thing to assert is that it
    // remains the ONLY caller of the bypass.
    let users = code_only(
        &std::fs::read_to_string(source("src/routes/users.rs")).expect("routes/users.rs"),
    );
    assert!(
        users.contains("Session::from_cookies(cookies)"),
        "stop-view-as no longer reads the cookie directly, so it cannot work while impersonating"
    );

    let bypass: usize = ["src/auth.rs", "src/routes/users.rs", "src/routes/rooms.rs"]
        .iter()
        .map(|f| {
            code_only(&std::fs::read_to_string(source(f)).unwrap_or_default())
                .matches("from_cookies(")
                .count()
        })
        .sum();
    assert!(
        bypass <= 3,
        "`Session::from_cookies` has grown callers: it skips the read-only refusal, and every use \
         outside stop-view-as needs justifying ({bypass} found)"
    );

    // And an impersonated session must not carry admin rights: seeing admin-only affordances
    // through somebody else's eyes is the opposite of what the feature answers.
    assert!(
        users.contains("is_admin: false"),
        "the impersonated session keeps its admin flag, so it does not show what that user sees"
    );
}

/// Rust source with `//` line comments removed, keeping newlines so the shape is unchanged.
///
/// For lints that assert something about *code*: a comment explaining a rule names the identifiers
/// the rule is about, so an assertion over raw source reads the explanation as though it were the
/// thing explained. That has produced a vacuous pass twice here and a false failure once.
fn code_only(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// **Every `popovertarget` names an `id` that exists in the same template.**
///
/// A mismatched pair is the quietest possible failure: the button renders, it is focusable, it has
/// a tooltip, and clicking it does *nothing at all*. No console error, no network request, no
/// visual change — an operator would reasonably conclude the sanction had been applied and moved
/// on. The browser gives no feedback because a `popovertarget` pointing at nothing is not an error,
/// it is just a reference to an element that is not there.
///
/// These ids are **templated** (`ban-{{ row.id }}`), so the check is a string comparison of the
/// expressions rather than of rendered output — which is what makes it a source lint. Rendering
/// would work too, but only for the rows a test happened to build.
#[test]
fn every_popover_button_points_at_a_popover_that_exists() {
    let mut offenders = Vec::new();
    let mut pairs = 0;

    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        let name = label(&path);

        let ids: Vec<&str> = attribute_values(&source, "id=\"").collect();
        for target in attribute_values(&source, "popovertarget=\"") {
            pairs += 1;
            if !ids.contains(&target) {
                offenders.push(format!(
                    "{name}: popovertarget=\"{target}\" names no element in this template"
                ));
            }
        }

        // The other half: a popover nothing can open is dead markup, and on a page where the
        // overlay carries the form, it is a control the operator cannot reach at all.
        let targets: Vec<&str> = attribute_values(&source, "popovertarget=\"").collect();
        for (at, _) in source.match_indices("<div popover id=\"") {
            let id = source[at + "<div popover id=\"".len()..]
                .split('"')
                .next()
                .unwrap_or_default();
            if !targets.contains(&id) {
                offenders.push(format!(
                    "{name}: popover id=\"{id}\" has no button that opens it"
                ));
            }
        }
    }

    assert!(
        pairs >= 4,
        "only {pairs} popover buttons found -- this lint is no longer looking at anything"
    );
    assert!(
        offenders.is_empty(),
        "a popover button that names nothing renders, focuses, and silently does nothing:\n  {}",
        offenders.join("\n  ")
    );
}

/// Every value of `<attr>="..."` in the source, as borrowed slices.
fn attribute_values<'a>(source: &'a str, attribute: &'a str) -> impl Iterator<Item = &'a str> {
    source.match_indices(attribute).map(move |(at, _)| {
        source[at + attribute.len()..]
            .split('"')
            .next()
            .unwrap_or_default()
    })
}
